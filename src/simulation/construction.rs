use crate::economy::agent::EconomicAgent;
use crate::pathfinding::chunk_graph::ChunkGraph;
use crate::pathfinding::chunk_router::ChunkRouter;
use crate::pathfinding::connectivity::ChunkConnectivity;
use crate::simulation::faction::{
    FactionChief, FactionData, FactionMember, FactionRegistry, FactionTechs, StorageTileMap, SOLO,
};
use crate::simulation::goals::{yield_for_maintenance_boundary, MAINTENANCE_WORK_SLICE_TICKS};
use crate::simulation::jobs::{
    record_progress_filtered, release_claimant, ClaimTarget, HaulSource, JobBoard, JobClaim,
    JobCompletedEvent, JobKind, JobProgress,
};
use crate::simulation::lod::LodLevel;
use crate::simulation::needs::Needs;
use crate::simulation::person::{AiState, Person, PersonAI, Profession, UNEMPLOYED_TASK_KIND};
use crate::simulation::schedule::{BucketSlot, SimClock};
use crate::simulation::skills::{SkillKind, Skills};
use crate::simulation::tasks::{assign_task_with_routing, TaskKind};
use crate::simulation::technology::{
    current_era, Era, TechId, BRIDGE_BUILDING, BRONZE_CASTING, BRONZE_TOOLS, CITY_STATE_ORG,
    COPPER_TOOLS, COPPER_WORKING, FIRED_POTTERY, FIRE_MAKING, FLINT_KNAPPING, GRANARY,
    LONG_DIST_TRADE, LOOM_WEAVING, MONUMENTAL_BUILDING, PERM_SETTLEMENT, PORTABLE_DWELLINGS,
    PROFESSIONAL_ARMY, SACRED_RITUAL, TECH_TREE, WELL_DIGGING,
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
pub struct BedMap(pub AHashMap<(i32, i32), Entity>);

/// Maps tile positions to wall entities placed there.
#[derive(Resource, Default)]
pub struct WallMap(pub AHashMap<(i32, i32), Entity>);

/// Maps tile positions to campfire entities placed there.
#[derive(Resource, Default)]
pub struct CampfireMap(pub AHashMap<(i32, i32), Entity>);

/// Maps tile positions to active Blueprint entities (faction build reservations).
#[derive(Resource, Default)]
pub struct BlueprintMap(pub AHashMap<(i32, i32), Entity>);

/// Queue of (faction_id, building_tile, home_tile) tuples populated by
/// `construction_system` when a structure finalises and by the planner when a
/// new road spine is laid out. `road_carve_system` drains it each tick and
/// runs Bresenham from the building tile back to the home tile, marking each
/// passable, non-Wall tile as `TileKind::Road`.
#[derive(Resource, Default)]
pub struct RoadCarveQueue(pub Vec<(u32, (i32, i32), (i32, i32))>);

/// Per-door tracking: stores the door entity and its current open state so
/// `has_los` can query door state by tile without joining a Bevy query.
#[derive(Clone, Copy)]
pub struct DoorEntry {
    pub entity: Entity,
    pub open: bool,
}

/// Maps tile positions to door entries placed there.
#[derive(Resource, Default)]
pub struct DoorMap(pub AHashMap<(i32, i32), DoorEntry>);

/// Maps tile positions to workbench entities placed there.
#[derive(Resource, Default)]
pub struct WorkbenchMap(pub AHashMap<(i32, i32), Entity>);

/// Maps tile positions to loom entities placed there.
#[derive(Resource, Default)]
pub struct LoomMap(pub AHashMap<(i32, i32), Entity>);

/// Maps tile positions to table entities placed there.
#[derive(Resource, Default)]
pub struct TableMap(pub AHashMap<(i32, i32), Entity>);

/// Maps tile positions to chair entities placed there.
#[derive(Resource, Default)]
pub struct ChairMap(pub AHashMap<(i32, i32), Entity>);

/// Maps tile positions to granary entities placed there.
#[derive(Resource, Default)]
pub struct GranaryMap(pub AHashMap<(i32, i32), Entity>);

/// Maps tile positions to shrine entities placed there.
#[derive(Resource, Default)]
pub struct ShrineMap(pub AHashMap<(i32, i32), Entity>);

/// Maps tile positions to market entities placed there.
#[derive(Resource, Default)]
pub struct MarketMap(pub AHashMap<(i32, i32), Entity>);

/// Maps tile positions to barracks entities placed there.
#[derive(Resource, Default)]
pub struct BarracksMap(pub AHashMap<(i32, i32), Entity>);

/// Maps tile positions to monument entities placed there.
#[derive(Resource, Default)]
pub struct MonumentMap(pub AHashMap<(i32, i32), Entity>);

/// Maps tile positions to bridge entities placed there. Bridge entities own
/// their tile slot exclusively (one Bridge per River cell). Lookup avoids
/// touching `chunk_map` for deconstruct / inspector paths.
#[derive(Resource, Default)]
pub struct BridgeMap(pub AHashMap<(i32, i32), Entity>);

/// Maps tile positions to dam entities placed there. Mirrors `BridgeMap`:
/// the `Dam` entity is the durable truth (faction-owned, refundable,
/// restamped onto fresh chunks by `restamp_runtime_water_on_chunk_load`);
/// `TileKind::Dam` is its cache projection. One Dam per cell.
#[derive(Resource, Default)]
pub struct DamMap(pub AHashMap<(i32, i32), Entity>);

/// Maps tile positions to well entities placed there. Read by the drink
/// HTN dispatcher (`htn_drink_dispatch_system`) for the well-priority scan,
/// and by `organic_settlement` for placement scoring + anchor emission.
#[derive(Resource, Default)]
pub struct WellMap(pub AHashMap<(i32, i32), Entity>);

/// Lined public well. Drinks fire from a chebyshev-adjacent tile via
/// `DrinkSource::Well`. No tile rewrite — sits on whatever surface was
/// underneath.
#[derive(Component, Clone, Copy, Debug)]
pub struct Well {
    pub faction_id: u32,
}

/// Constructed timber span. The tile slot is mutated to `TileKind::Bridge`
/// at finalize; on deconstruct we read `restore_tile` to put the original
/// tile back (always `River` for the current build path, but stored
/// explicitly to keep deconstruct correct if the rule ever loosens).
#[derive(Component, Clone, Copy, Debug)]
pub struct Bridge {
    pub faction_id: u32,
    pub tile: (i32, i32),
    pub restore_tile: TileKind,
}

/// Constructed dam barrier. Mirrors `Bridge`: the tile slot is mutated to
/// `TileKind::Dam` at finalize and a hydrology barrier is registered in
/// `RuntimeWater` at the crest (`crest_z`). On deconstruct we restore
/// `restore_tile` and clear the barrier so the impounded water drains.
/// The entity is the durable truth — the chunk's `Dam` tile is restamped
/// from `DamMap` on every reload.
#[derive(Component, Clone, Copy, Debug)]
pub struct Dam {
    pub faction_id: u32,
    pub tile: (i32, i32),
    pub restore_tile: TileKind,
    /// Crest height in Z (the dam tile's surface Z). The fluid sim treats
    /// the cell as a wall below this level — water pools upstream to it.
    pub crest_z: i8,
}

/// Display name for any constructed entity (Wall, Bed, Door, Blueprint, …).
/// The hover panel reads this directly so adding a new structure variant
/// only needs to set the right label at its spawn site — no inspector edits.
#[derive(Component, Copy, Clone, Debug)]
pub struct StructureLabel(pub &'static str);

/// Tile-keyed reverse index over every entity carrying `StructureLabel`.
/// Maintained by component lifecycle hooks (`on_structure_label_add` /
/// `on_structure_label_remove`) so spawn/despawn paths stay untouched.
#[derive(Resource, Default)]
pub struct StructureIndex(pub AHashMap<(i32, i32), Entity>);

pub fn on_structure_label_add(
    mut world: bevy::ecs::world::DeferredWorld<'_>,
    entity: Entity,
    _: bevy::ecs::component::ComponentId,
) {
    let Some(transform) = world.get::<Transform>(entity).copied() else {
        return;
    };
    let tile = crate::world::terrain::world_to_tile(transform.translation.truncate());
    let mut index = world.resource_mut::<StructureIndex>();
    index.0.insert(tile, entity);
}

pub fn on_structure_label_remove(
    mut world: bevy::ecs::world::DeferredWorld<'_>,
    entity: Entity,
    _: bevy::ecs::component::ComponentId,
) {
    let Some(transform) = world.get::<Transform>(entity).copied() else {
        return;
    };
    let tile = crate::world::terrain::world_to_tile(transform.translation.truncate());
    let mut index = world.resource_mut::<StructureIndex>();
    if index.0.get(&tile).copied() == Some(entity) {
        index.0.remove(&tile);
    }
}

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
    pub bridge_map: ResMut<'w, BridgeMap>,
    pub dam_map: ResMut<'w, DamMap>,
    pub well_map: ResMut<'w, WellMap>,
    /// Persistent runtime water (Phase 3). Dam finalize registers its crest
    /// barrier here; deconstruct clears it. Bundled to stay under the
    /// 16-param system cap.
    pub runtime_water: ResMut<'w, crate::world::water_runtime::RuntimeWater>,
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
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
/// when pathfinding gains faction context. `dir` is the cardinal the door
/// opens onto; `doormat_tile` is the protected outside tile (one step in
/// `dir`) reserved in `DoormatReservations` and carved as `Road`. Both
/// `dir` and `doormat_tile` are read by inspector hover / future faction-aware
/// pathfinding; the doormat reservation is freed in `Door::on_remove` by
/// matching on the entity id, not by reading these fields.
#[derive(Component)]
#[allow(dead_code)]
pub struct Door {
    pub faction_id: u32,
    pub open: bool,
    pub tier: DoorTier,
    pub dir: crate::simulation::land::TileEdge,
    pub doormat_tile: (i32, i32),
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BuildSiteKind {
    Wall(WallMaterial),
    Door,
    Bed,
    /// Nomadic per-person Bed equivalent. Deploys as a 1-tile `Bed { tier:
    /// Crude }` carrying a `Deployable` marker; on migration it packs back
    /// into a `bedroll` good in the owner's inventory. Tech-gate-free
    /// (Paleolithic-available). See `pack_deploy.rs`.
    Bedroll,
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
    /// Sticks-and-leaves shelter. Deployed-only; on migration teardown
    /// `pack_deployable` (Phase 8) drops 50% of the input wood as
    /// `GroundItem`s. No tech gate.
    Tent,
    /// Felt-and-lattice packable shelter. Packs to `Good::PackedYurt` on
    /// migration; re-pitches at the new camp at zero material cost.
    /// Tech-gated on `PORTABLE_DWELLINGS` (Neolithic).
    Yurt,
    /// Open-trench latrine. Cheap (wood + stone surround). When a `Latrine`
    /// entity sits within `LATRINE_ROUTING_RADIUS` of an agent's defecation
    /// tile, the spawned `WastePile` is tagged `LatrineContained` and
    /// contributes a fraction of its raw intensity to `SanitationMap` —
    /// the village's contamination signature stays bounded around the
    /// latrine rather than smearing across living space.
    Latrine,
    /// Timber span over a single `TileKind::River` tile. Finalisation
    /// rewrites the tile to `TileKind::Bridge`; deconstruction restores
    /// the original `River` tile via the `Bridge` component's
    /// `restore_tile`. Tech-gated on `BRIDGE_BUILDING` (Chalcolithic).
    Bridge,
    /// Dam barrier across a single watercourse (`River`/`Water`) tile.
    /// Finalisation rewrites the tile to `TileKind::Dam` and registers a
    /// hydrology barrier in `RuntimeWater` at the crest; deconstruction
    /// restores the prior tile via `Dam::restore_tile` and clears the
    /// barrier (impounded water drains). Tech-gated on `BRIDGE_BUILDING`
    /// (v1 reuses it; a dedicated `DAM_BUILDING` tech is v2).
    Dam,
    /// Lined public well. 1-tile, impassable — agents drink from a
    /// chebyshev-adjacent tile via `DrinkSource::Well`. Tech-gated on
    /// `WELL_DIGGING` (Neolithic). No tile rewrite on finalize/deconstruct.
    Well,
}

/// Marker component on tile entities representing portable shelter.
/// Auras / sleep-comfort buffs land in a follow-on; for now this just
/// tags the entity for inspector hover and pack/deploy filtering.
#[derive(Component, Clone, Copy, Debug)]
pub struct TentShelter {
    pub tier: ShelterTier,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ShelterTier {
    Tent,
    Yurt,
}

impl ShelterTier {
    pub fn label(self) -> &'static str {
        match self {
            ShelterTier::Tent => "Tent",
            ShelterTier::Yurt => "Yurt",
        }
    }
}

impl BuildSiteKind {
    pub fn label(self) -> &'static str {
        match self {
            BuildSiteKind::Wall(mat) => mat.label(),
            BuildSiteKind::Door => "Door",
            BuildSiteKind::Bed => "Bed",
            BuildSiteKind::Bedroll => "Bedroll",
            BuildSiteKind::Campfire => "Campfire",
            BuildSiteKind::Workbench => "Workbench",
            BuildSiteKind::Loom => "Loom",
            BuildSiteKind::Table => "Table",
            BuildSiteKind::Chair => "Chair",
            BuildSiteKind::Granary => "Granary",
            BuildSiteKind::Shrine => "Shrine",
            BuildSiteKind::Market => "Market",
            BuildSiteKind::Barracks => "Barracks",
            BuildSiteKind::Monument => "Monument",
            BuildSiteKind::Tent => "Tent",
            BuildSiteKind::Yurt => "Yurt",
            BuildSiteKind::Latrine => "Latrine",
            BuildSiteKind::Bridge => "Bridge",
            BuildSiteKind::Dam => "Dam",
            BuildSiteKind::Well => "Well",
        }
    }

    /// True for blueprints whose anchor sits on impassable water and whose
    /// workers must stand on an adjacent passable bank tile. Today: only
    /// `Bridge`. Used by `is_clear_footprint`-style checks to bypass the
    /// passability gate, and by `work_stand_for` to route gather/build legs.
    pub fn is_water_anchored(self) -> bool {
        matches!(self, BuildSiteKind::Bridge | BuildSiteKind::Dam)
    }
}

/// A single ingredient slot inside a `Blueprint`. `needed` is fixed at spawn
/// from the recipe; `deposited` advances as workers contribute the matching
/// resource.
#[derive(Clone, Copy, Debug, Default)]
pub struct GoodNeed {
    pub resource_id: crate::economy::resource_catalog::ResourceId,
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
    pub tile: (i32, i32),
    /// Z-level at which the placed structure should sit. All blueprints
    /// belonging to one building share this value so the walls form a
    /// coherent floor instead of scattering across per-tile surface_z.
    pub target_z: i8,
    pub deposits: [GoodNeed; MAX_BUILD_INPUTS],
    pub deposit_count: u8,
    pub build_progress: u8,
    /// Obstacle entities (plants, etc.) that workers must clear before
    /// construction can accumulate `build_progress`. Maintained by the
    /// `ClearObstacle` task executor; populated at blueprint creation
    /// via `obstacle::scan_footprint`.
    pub pending_clear: Vec<Entity>,
    /// Cardinal the door will open onto when `kind == BuildSiteKind::Door`.
    /// `None` for non-door blueprints (and for legacy door blueprints whose
    /// direction wasn't sourced from a frontage / road halo).
    pub door_dir: Option<crate::simulation::land::TileEdge>,
    /// Adjacent passable bank tile for water-anchored blueprints
    /// (`BuildSiteKind::Bridge`). Workers route here for haul/build legs
    /// because `tile` itself sits on impassable `River`. `None` for every
    /// other kind — the executor uses `tile` directly.
    pub work_stand: Option<(i32, i32)>,
    /// Entity that authorized this build (chief or architect). `None` for
    /// seed-time direct emission. Read at completion to call
    /// `record_tech_use` so practice diffuses tech adoption. Snapshot —
    /// survives the poster's death.
    pub posted_by: Option<Entity>,
    /// Poster's `Learned` bitset at intent-spawn time. Read at completion
    /// by tier helpers (`best_bed_for` etc.) so the structure upgrades to
    /// the design tier even if the build paused across succession.
    pub design_techs: FactionTechs,
}

impl Blueprint {
    /// Build a blueprint pre-filled from `recipe_for(kind)`.
    pub fn new(
        faction_id: u32,
        personal_owner: Option<Entity>,
        kind: BuildSiteKind,
        tile: (i32, i32),
        target_z: i8,
    ) -> Self {
        let recipe = recipe_for(kind);
        let mut deposits = [GoodNeed::default(); MAX_BUILD_INPUTS];
        let count = recipe.inputs.len().min(MAX_BUILD_INPUTS);
        for (i, &(rid, qty)) in recipe.inputs.iter().take(count).enumerate() {
            deposits[i] = GoodNeed {
                resource_id: rid,
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
            pending_clear: Vec::new(),
            door_dir: None,
            work_stand: None,
            posted_by: None,
            design_techs: FactionTechs::default(),
        }
    }

    /// Stamp the (entity, learned-bitset) of the poster onto an existing
    /// blueprint. No-op when `author` is None — preserves the legacy
    /// `Blueprint::new` defaults (posted_by None, design_techs empty).
    pub fn with_author(mut self, author: Option<BlueprintAuthor>) -> Self {
        if let Some(a) = author {
            self.posted_by = Some(a.posted_by);
            self.design_techs = a.design_techs;
        }
        self
    }

    /// Same as `new`, but stamps the authoring poster and their `Learned`
    /// bitset. Used by runtime intent emission once the poster pool exists;
    /// seed-time and legacy call sites stay on `new` (posted_by = None).
    pub fn new_with_poster(
        faction_id: u32,
        personal_owner: Option<Entity>,
        kind: BuildSiteKind,
        tile: (i32, i32),
        target_z: i8,
        posted_by: Option<Entity>,
        design_techs: FactionTechs,
    ) -> Self {
        let mut bp = Self::new(faction_id, personal_owner, kind, tile, target_z);
        bp.posted_by = posted_by;
        bp.design_techs = design_techs;
        bp
    }

    /// Tile workers should route to. For water-anchored blueprints
    /// (`BuildSiteKind::Bridge`) this is the cached bank `work_stand`;
    /// everyone else routes to the anchor `tile`.
    #[inline]
    pub fn worker_target_tile(&self) -> (i32, i32) {
        self.work_stand.unwrap_or(self.tile)
    }

    /// Builder: stamp the door's opening cardinal. Caller must only set this
    /// for `BuildSiteKind::Door` blueprints; ignored for other kinds.
    pub fn with_door_dir(mut self, dir: crate::simulation::land::TileEdge) -> Self {
        self.door_dir = Some(dir);
        self
    }

    /// `true` when every plant/obstacle in the footprint has been cleared
    /// and `construction_system` is allowed to accumulate work_progress.
    pub fn obstacles_cleared(&self) -> bool {
        self.pending_clear.is_empty()
    }

    pub fn is_satisfied(&self) -> bool {
        for i in 0..self.deposit_count as usize {
            if self.deposits[i].deposited < self.deposits[i].needed {
                return false;
            }
        }
        true
    }

    /// True when the slot for `resource_id` is filled, or when the blueprint
    /// has no slot for that resource (trivially nothing to deliver).
    pub fn slot_satisfied(
        &self,
        resource_id: crate::economy::resource_catalog::ResourceId,
    ) -> bool {
        for i in 0..self.deposit_count as usize {
            if self.deposits[i].resource_id == resource_id {
                return self.deposits[i].deposited >= self.deposits[i].needed;
            }
        }
        true
    }
}

/// Who authorised a runtime construction and what `Learned` set they were
/// carrying when they authored it. Threaded through `spawn_intent` ⇒
/// `plan_building` ⇒ `plan_composite_building` ⇒ deferred `PendingFootprint`
/// so every blueprint a poster emits carries their snapshot. The snapshot
/// freezes tier picks at intent time (chief succession or architect death
/// mid-build doesn't change the design) and feeds `record_tech_use` at
/// completion so practice diffuses tech adoption.
///
/// `None` callers (seed paths, legacy emission sites) keep producing
/// blueprints with `posted_by = None`, `design_techs = FactionTechs(0)`,
/// which `construction_system` interprets as "not practice — skip
/// diffusion." This is the forward-compatible bridge: a call site can
/// adopt `BlueprintAuthor` without changing the rest of the pipeline.
#[derive(Clone, Copy, Debug)]
pub struct BlueprintAuthor {
    pub posted_by: Entity,
    pub design_techs: FactionTechs,
}

impl BlueprintAuthor {
    pub fn new(posted_by: Entity, design_techs: FactionTechs) -> Self {
        Self {
            posted_by,
            design_techs,
        }
    }
}

// ── Build recipes ─────────────────────────────────────────────────────────────

/// Description of how to build a single structure kind: ingredients,
/// labour ticks, optional tech gate, and what is refunded on deconstruction.
/// Inputs/refunds are `ResourceId`-keyed; the recipe table is built lazily
/// via [`build_recipes`] because `ResourceId`s resolve through the runtime
/// catalog and can't be expressed in `const`.
pub struct BuildRecipe {
    pub name: &'static str,
    pub inputs: Vec<(crate::economy::resource_catalog::ResourceId, u8)>,
    pub work_ticks: u8,
    pub tech_gate: Option<TechId>,
    pub deconstruct_refund: Vec<(crate::economy::resource_catalog::ResourceId, u8)>,
}

/// Stable index into the lazy build-recipe table. One entry per
/// `BuildSiteKind` variant (wall variants flatten via `WallMaterial`).
#[derive(Copy, Clone, Debug)]
#[repr(usize)]
enum BuildRecipeIdx {
    Palisade = 0,
    WattleDaub,
    StoneWall,
    Mudbrick,
    CutStone,
    Workbench,
    Loom,
    Table,
    Chair,
    Door,
    Bed,
    Campfire,
    Granary,
    Shrine,
    Market,
    Barracks,
    Monument,
    Bedroll,
    Tent,
    Yurt,
    Latrine,
    Bridge,
    Well,
    // Appended last so existing discriminants (= vec positions) stay stable.
    Dam,
}

fn build_recipes_table() -> Vec<BuildRecipe> {
    use crate::economy::core_ids;
    let _ = core_ids::catalog();
    let wood = core_ids::wood();
    let stone = core_ids::stone();
    let grain = core_ids::grain();
    let skin = core_ids::skin();
    let bedroll = core_ids::bedroll();
    let packed_yurt = core_ids::packed_yurt();

    vec![
        BuildRecipe {
            name: "Palisade Wall",
            inputs: vec![(wood, 2)],
            work_ticks: 60,
            tech_gate: None,
            deconstruct_refund: vec![(wood, 1)],
        },
        BuildRecipe {
            name: "Wattle & Daub Wall",
            inputs: vec![(wood, 2), (grain, 1)],
            work_ticks: 70,
            tech_gate: Some(PERM_SETTLEMENT),
            deconstruct_refund: vec![(wood, 1)],
        },
        BuildRecipe {
            name: "Stone Wall",
            inputs: vec![(stone, 3)],
            work_ticks: 90,
            tech_gate: Some(FLINT_KNAPPING),
            deconstruct_refund: vec![(stone, 2)],
        },
        BuildRecipe {
            name: "Mudbrick Wall",
            inputs: vec![(stone, 2), (wood, 1)],
            work_ticks: 80,
            tech_gate: Some(FIRED_POTTERY),
            deconstruct_refund: vec![(stone, 1)],
        },
        BuildRecipe {
            name: "Cut Stone Wall",
            inputs: vec![(stone, 4)],
            work_ticks: 120,
            tech_gate: Some(MONUMENTAL_BUILDING),
            deconstruct_refund: vec![(stone, 3)],
        },
        BuildRecipe {
            name: "Workbench",
            inputs: vec![(wood, 3), (stone, 1)],
            work_ticks: 60,
            tech_gate: Some(FLINT_KNAPPING),
            deconstruct_refund: vec![(wood, 2)],
        },
        BuildRecipe {
            name: "Loom",
            inputs: vec![(wood, 4)],
            work_ticks: 70,
            tech_gate: Some(LOOM_WEAVING),
            deconstruct_refund: vec![(wood, 2)],
        },
        BuildRecipe {
            name: "Table",
            inputs: vec![(wood, 3)],
            work_ticks: 50,
            tech_gate: None,
            deconstruct_refund: vec![(wood, 2)],
        },
        BuildRecipe {
            name: "Chair",
            inputs: vec![(wood, 2)],
            work_ticks: 40,
            tech_gate: None,
            deconstruct_refund: vec![(wood, 1)],
        },
        BuildRecipe {
            name: "Door",
            inputs: vec![(wood, 2)],
            work_ticks: 50,
            tech_gate: None,
            deconstruct_refund: vec![(wood, 1)],
        },
        BuildRecipe {
            name: "Bed",
            inputs: vec![(wood, 3)],
            work_ticks: 80,
            tech_gate: None,
            deconstruct_refund: vec![(wood, 2)],
        },
        BuildRecipe {
            name: "Campfire",
            inputs: vec![(wood, 2)],
            work_ticks: 40,
            tech_gate: Some(FIRE_MAKING),
            deconstruct_refund: vec![(wood, 1)],
        },
        BuildRecipe {
            name: "Granary",
            inputs: vec![(wood, 4), (stone, 2)],
            work_ticks: 120,
            tech_gate: Some(GRANARY),
            deconstruct_refund: vec![(wood, 2), (stone, 1)],
        },
        BuildRecipe {
            name: "Shrine",
            inputs: vec![(stone, 3), (wood, 2)],
            work_ticks: 140,
            tech_gate: Some(SACRED_RITUAL),
            deconstruct_refund: vec![(stone, 1), (wood, 1)],
        },
        BuildRecipe {
            name: "Market",
            inputs: vec![(wood, 5), (stone, 2)],
            work_ticks: 160,
            tech_gate: Some(LONG_DIST_TRADE),
            deconstruct_refund: vec![(wood, 2), (stone, 1)],
        },
        BuildRecipe {
            name: "Barracks",
            inputs: vec![(wood, 4), (stone, 3)],
            work_ticks: 180,
            tech_gate: Some(PROFESSIONAL_ARMY),
            deconstruct_refund: vec![(wood, 2), (stone, 1)],
        },
        BuildRecipe {
            name: "Monument",
            inputs: vec![(stone, 6), (wood, 2)],
            work_ticks: 220,
            tech_gate: Some(MONUMENTAL_BUILDING),
            deconstruct_refund: vec![(stone, 3), (wood, 1)],
        },
        // Nomadic kit. Bedroll is a per-person packable Bed: cheap, no tech
        // gate, deploys as `Bed { tier: Crude }` carrying `Deployable`.
        // Settled factions can also build them (rare, but harmless).
        BuildRecipe {
            name: "Bedroll",
            inputs: vec![(skin, 1), (wood, 2)],
            work_ticks: 30,
            tech_gate: None,
            deconstruct_refund: vec![(bedroll, 1)],
        },
        BuildRecipe {
            name: "Tent",
            inputs: vec![(wood, 6), (skin, 3)],
            work_ticks: 50,
            tech_gate: None,
            // Sticks-and-leaves teardown: half the wood comes back as
            // GroundItems on migration; the rest stays at the old camp.
            // (Phase 8 reads `Deployable.refund_pct = 0.5` directly so this
            // refund vec serves only the player-deconstruct path.)
            deconstruct_refund: vec![(wood, 3)],
        },
        BuildRecipe {
            name: "Yurt",
            inputs: vec![(wood, 8), (skin, 6)],
            work_ticks: 90,
            tech_gate: Some(PORTABLE_DWELLINGS),
            // Player-deconstruct returns the packed-yurt good directly.
            deconstruct_refund: vec![(packed_yurt, 1)],
        },
        // Open-trench latrine. Wood for the surround, a stone slab as a
        // step. No tech gate — pit latrines predate writing. Mirrors the
        // Campfire shape (cheap + small) since it's a personal hygiene
        // structure, not a workshop. Deconstruct returns one wood.
        BuildRecipe {
            name: "Latrine",
            inputs: vec![(wood, 2), (stone, 1)],
            work_ticks: 50,
            tech_gate: None,
            deconstruct_refund: vec![(wood, 1)],
        },
        // Timber bridge spanning one river tile. Player-deconstruct returns
        // half the inputs; the actual drop site is the nearest passable bank
        // tile, not the river cell itself (see deconstruct path).
        BuildRecipe {
            name: "Timber Bridge",
            inputs: vec![(wood, 4), (stone, 2)],
            work_ticks: 120,
            tech_gate: Some(BRIDGE_BUILDING),
            deconstruct_refund: vec![(wood, 2), (stone, 1)],
        },
        // Lined public well. Stone surround over a wooden frame. Tech-gated
        // on `WELL_DIGGING` (Neolithic). Mirrors the Granary recipe shape
        // (heavier than a workshop) since the shaft + lining is real labor.
        BuildRecipe {
            name: "Well",
            inputs: vec![(stone, 4), (wood, 2)],
            work_ticks: 120,
            tech_gate: Some(WELL_DIGGING),
            deconstruct_refund: vec![(stone, 2), (wood, 1)],
        },
        // Dam barrier. Heavier than a bridge — it holds back water, not
        // foot traffic — so more stone + longer work. v1 reuses
        // `BRIDGE_BUILDING` (a dedicated `DAM_BUILDING` tech is v2).
        // Player-deconstruct returns half; drop site is the nearest
        // passable bank (same as Bridge — see deconstruct path).
        BuildRecipe {
            name: "Dam",
            inputs: vec![(stone, 6), (wood, 4)],
            work_ticks: 180,
            tech_gate: Some(BRIDGE_BUILDING),
            deconstruct_refund: vec![(stone, 3), (wood, 2)],
        },
    ]
}

fn build_recipes() -> &'static [BuildRecipe] {
    static TABLE: std::sync::OnceLock<Vec<BuildRecipe>> = std::sync::OnceLock::new();
    TABLE.get_or_init(build_recipes_table).as_slice()
}

/// How obtainable a single construction-input resource is for a faction at
/// chief-decision time. Drives the era-aware material selector and the
/// `HaulSource` stamped on Phase 3c Haul postings.
///
/// `stored` is **deposited faction storage only** (never agent inventories —
/// posting/candidate scoring must not double-count carried goods). `inventory`
/// is informational (faction `supply - stored`). `raw_gatherable` is the
/// coarse "is there a known accessible resource cluster" signal — clusters
/// carry no quantity, so this is boolean, not a reserve estimate.
#[derive(Clone, Copy, Debug)]
#[allow(dead_code)] // consumed by the selector/classifier in Step 2+
pub struct ResourceAvailability {
    pub stored: u32,
    pub inventory: u32,
    pub market_stock: f32,
    pub market_price: f32,
    pub affordable_qty: u32,
    pub raw_gatherable: bool,
    pub scarcity: Scarcity,
}

/// Scarcity tier for one construction input, relative to the recipe quantity
/// needed for one structure. `Tight` means short in storage but raw-gatherable
/// (the existing gather pipeline resolves it — no procurement). `Scarce` means
/// not stored / not gatherable but affordably procurable at the market node.
/// `Unavailable` means none of the above — substitute down the ladder, then
/// emergency shelter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)] // consumed by the selector/classifier in Step 2+
pub enum Scarcity {
    Available,
    Tight,
    Scarce,
    Unavailable,
}

/// Result of the era-aware wall-material selector. `Material` carries the
/// chosen ladder rung plus how its scarce inputs (if any) must be acquired.
/// `EmergencyShelter` means every ladder rung's inputs are `Unavailable` and
/// not procurable — the caller emits era-appropriate emergency bedding instead
/// of a walled house.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum WallSelection {
    Material {
        mat: WallMaterial,
        source: HaulSource,
    },
    EmergencyShelter,
}

/// Most advanced wall material a faction's tech bitset allows. Used by the
/// chief to upgrade defensive walls automatically; the player may still pick
/// any unlocked material via the right-click menu.
pub fn best_wall_material(techs: &FactionTechs) -> WallMaterial {
    // Stone walls require at least Chalcolithic copper-working (a proxy for the
    // labor and tool sophistication needed for dressed masonry). Pre-Chalcolithic
    // hut walls fall back to Mudbrick / Wattle & Daub / Palisade — historically
    // appropriate for Neolithic and earlier.
    if techs.has(MONUMENTAL_BUILDING) {
        WallMaterial::CutStone
    } else if techs.has(COPPER_WORKING) {
        WallMaterial::Stone
    } else if techs.has(FIRED_POTTERY) {
        WallMaterial::Mudbrick
    } else if techs.has(PERM_SETTLEMENT) {
        WallMaterial::WattleDaub
    } else {
        WallMaterial::Palisade
    }
}

/// Wall materials in **tech-progression order** (low → high tech). This is
/// NOT the `WallMaterial` enum discriminant order (there Stone=2, Mudbrick=3
/// are swapped relative to their tech gates). `best_wall_material` walks tech
/// gates in exactly this order; `select_wall_material` steps *down* it when a
/// rung's recipe input is unavailable.
const WALL_LADDER_BY_TECH: [WallMaterial; 5] = [
    WallMaterial::Palisade,
    WallMaterial::WattleDaub,
    WallMaterial::Mudbrick,
    WallMaterial::Stone,
    WallMaterial::CutStone,
];

/// Per-rid availability snapshot for every construction input the wall ladder
/// (and doors) may require. Built once per chief tick by
/// `classify_construction_materials`; consumed by `select_wall_material` and
/// (Step 3) Phase 3c Haul-source stamping.
#[derive(Clone, Debug, Default)]
#[allow(dead_code)] // populated/consumed at chief cadence in Step 3+
pub struct MaterialAvailabilityView {
    by_rid: AHashMap<crate::economy::resource_catalog::ResourceId, ResourceAvailability>,
}

#[allow(dead_code)] // accessors used by Step 3 wiring + Step 2 unit tests
impl MaterialAvailabilityView {
    pub fn insert(
        &mut self,
        rid: crate::economy::resource_catalog::ResourceId,
        av: ResourceAvailability,
    ) {
        self.by_rid.insert(rid, av);
    }

    pub fn get(
        &self,
        rid: crate::economy::resource_catalog::ResourceId,
    ) -> Option<&ResourceAvailability> {
        self.by_rid.get(&rid)
    }

    /// True until the chief-cadence classifier has run at least once. An
    /// empty view must be treated as "not yet classified" (pass `None` to
    /// `select_wall_material` → unconstrained / legacy `best_wall_material`),
    /// NOT as "everything Unavailable" — otherwise a band emits emergency
    /// beds during the first classification window even when materials are
    /// readily gatherable.
    pub fn is_empty(&self) -> bool {
        self.by_rid.is_empty()
    }

    fn scarcity_of(&self, rid: crate::economy::resource_catalog::ResourceId) -> Scarcity {
        // Absent = never classified = treat as Unavailable (conservative: the
        // selector will step down rather than emit an unfunded Market haul).
        self.by_rid
            .get(&rid)
            .map(|a| a.scarcity)
            .unwrap_or(Scarcity::Unavailable)
    }

    /// Per-slot `HaulSource` for Phase 3c: `Market` only when the resource is
    /// `Scarce` (not stored, not gatherable, affordably procurable).
    pub fn haul_source_for(&self, rid: crate::economy::resource_catalog::ResourceId) -> HaulSource {
        match self.by_rid.get(&rid) {
            Some(a) if a.scarcity == Scarcity::Scarce => HaulSource::Market {
                max_unit_price: a.market_price,
            },
            _ => HaulSource::Storage,
        }
    }
}

/// Classify one construction input against the quantity `need`ed for one
/// structure. `stored` is **deposited faction storage only**; `supply` is the
/// faction total (storage + agent inventories) used solely to fill the
/// informational `inventory` field — the scarcity decision keys on `stored`
/// (deposited-only, per the posting/candidate-scoring rule).
#[allow(dead_code)] // wired into chief-cadence classification in Step 3
pub fn classify_resource(
    stored: u32,
    supply: u32,
    market_stock: f32,
    market_price: f32,
    treasury_budget: f32,
    raw_gatherable: bool,
    need: u32,
) -> ResourceAvailability {
    let price = if market_price > 0.0 {
        market_price
    } else {
        1.0
    };
    let affordable_qty = if treasury_budget > 0.0 {
        ((treasury_budget / price).floor())
            .max(0.0)
            .min(market_stock.max(0.0)) as u32
    } else {
        0
    };
    let scarcity = if stored >= need {
        Scarcity::Available
    } else if raw_gatherable {
        // Short in storage but a known accessible cluster exists — the
        // existing gather pipeline resolves this, no procurement.
        Scarcity::Tight
    } else if affordable_qty >= need {
        Scarcity::Scarce
    } else {
        Scarcity::Unavailable
    };
    ResourceAvailability {
        stored,
        inventory: supply.saturating_sub(stored),
        market_stock,
        market_price: price,
        affordable_qty,
        raw_gatherable,
        scarcity,
    }
}

impl WallSelection {
    /// The chosen wall material, or `None` for `EmergencyShelter`.
    pub fn mat(self) -> Option<WallMaterial> {
        match self {
            WallSelection::Material { mat, .. } => Some(mat),
            WallSelection::EmergencyShelter => None,
        }
    }
}

/// Era-aware wall-material selector. `avail == None` (seed mode + the
/// wall-upgrade pass — materials there are stamped for free / are a deliberate
/// chief tier-bump) returns the pure tech-best rung verbatim, exactly matching
/// legacy `best_wall_material` behaviour. `Some(view)` applies **procure-
/// primary-first**: keep the highest tech rung that's buildable, market-haul
/// its scarce inputs; step down the tech ladder only when a rung has a truly
/// `Unavailable` input; return `EmergencyShelter` when every rung is blocked.
pub fn select_wall_material(
    techs: &FactionTechs,
    avail: Option<&MaterialAvailabilityView>,
) -> WallSelection {
    let top = best_wall_material(techs);
    let Some(view) = avail else {
        return WallSelection::Material {
            mat: top,
            source: HaulSource::Storage,
        };
    };
    let top_idx = WALL_LADDER_BY_TECH
        .iter()
        .position(|m| *m == top)
        .unwrap_or(0);
    for idx in (0..=top_idx).rev() {
        let mat = WALL_LADDER_BY_TECH[idx];
        let recipe = recipe_for(BuildSiteKind::Wall(mat));
        // Defensive: the era ladder isn't a strict prereq chain, so skip any
        // rung whose own tech gate the faction can't meet.
        if let Some(gate) = recipe.tech_gate {
            if !techs.has(gate) {
                continue;
            }
        }
        let mut unavailable = false;
        let mut market_src: Option<HaulSource> = None;
        for &(rid, qty) in &recipe.inputs {
            match view.scarcity_of(rid) {
                Scarcity::Available | Scarcity::Tight => {}
                Scarcity::Scarce => {
                    if market_src.is_none() {
                        market_src = Some(view.haul_source_for(rid));
                    }
                    let _ = qty;
                }
                Scarcity::Unavailable => {
                    unavailable = true;
                    break;
                }
            }
        }
        if unavailable {
            continue; // step down one tech rung
        }
        return WallSelection::Material {
            mat,
            source: market_src.unwrap_or(HaulSource::Storage),
        };
    }
    WallSelection::EmergencyShelter
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

fn techs_through_era(era: Era) -> FactionTechs {
    let mut techs = FactionTechs::default();
    let era_rank = era as u8;
    for def in TECH_TREE.iter() {
        if (def.era as u8) <= era_rank {
            techs.unlock(def.id);
        }
    }
    techs
}

/// sleepy-dove Phase 7: explicit, typed description of what a fresh
/// founder band is *given* at game start — distinct from what they have
/// *adopted*. Seeding is "given," not "practiced," so seed-emitted
/// structures carry `posted_by = None` and never call `record_tech_use`.
///
/// `from_era` resolves the era-appropriate tier ladder once. The seed
/// pipeline still threads a `FactionTechs` bitset into the shared
/// `generate_candidates` / `seed_apply_intent` path (so seed and runtime
/// emit the same intent stream); `seed_techs()` exposes it. This makes
/// the seed driver a named profile rather than an `Option<&FactionTechs>`
/// that quietly impersonates community adoption.
///
/// Note: `seed_prime_tech_adoption_system` is **retained**, not removed —
/// the audit found non-construction consumers (`nomad`, settlement spawn
/// scoring in `settlement.rs`, `lifecycle`, `ui/tech_panel`) still read
/// the primed `tech_adoption` / `community_adoption_bitset` at tick 0.
/// Construction seeding no longer depends on the prime (it drives tiers
/// from this profile's `seed_techs()`), but the prime stays for those.
#[derive(Clone, Copy, Debug)]
pub struct SeedConstructionProfile {
    pub era: Era,
    pub hearth_tier: HearthTier,
    pub bed_tier: BedTier,
    pub door_tier: DoorTier,
    pub workbench_tier: WorkbenchTier,
    /// `None` = no defensive walls at this era (Paleo/Meso band camps).
    pub wall_material: Option<WallMaterial>,
    seed_techs: FactionTechs,
}

impl SeedConstructionProfile {
    pub fn from_era(era: Era) -> Self {
        let techs = techs_through_era(era);
        // Walls only from Neolithic+ (Palisade unlocks at PERM_SETTLEMENT);
        // Paleo/Meso bands run the deterministic band-camp seeder which
        // emits no walls regardless.
        let wall_material = if (era as u8) >= (Era::Neolithic as u8) {
            // Seed mode: materials are stamped for free, so pass `None`
            // (unconstrained) — identical to legacy `best_wall_material`.
            select_wall_material(&techs, None).mat()
        } else {
            None
        };
        // Band-camp hearth tier reproduces today's explicit era table
        // (NOT `best_hearth_for`, which would hand Paleo a Ringed hearth
        // because FLINT_KNAPPING is a Paleolithic tech). Neo+ stamps its
        // Campfire via `best_hearth_for(seed_techs)` in the shared
        // pipeline regardless; this field only drives the deterministic
        // Paleo/Meso band-camp + nomad seeder.
        let hearth_tier = match era {
            Era::Paleolithic | Era::Mesolithic => HearthTier::Open,
            Era::Neolithic => HearthTier::Ringed,
            Era::Chalcolithic | Era::BronzeAge => HearthTier::Lined,
        };
        Self {
            era,
            hearth_tier,
            bed_tier: best_bed_for(&techs),
            door_tier: best_door_for(&techs),
            workbench_tier: best_workbench_for(&techs),
            wall_material,
            seed_techs: techs,
        }
    }

    /// The `FactionTechs` bitset threaded into the shared seed pipeline.
    /// Drives tier picks in `generate_candidates` / `seed_apply_intent`
    /// via the same `best_*_for` ladder the runtime chief uses.
    #[inline]
    pub fn seed_techs(&self) -> &FactionTechs {
        &self.seed_techs
    }
}

/// True if the faction has the tech needed for this wall material.
pub fn faction_can_build(kind: BuildSiteKind, techs: &FactionTechs) -> bool {
    match recipe_for(kind).tech_gate {
        Some(t) => techs.has(t),
        None => true,
    }
}

/// Techs that would gate this `BuildSiteKind`: the recipe's `tech_gate`
/// only. Tier-driving techs (wall material, bed/door/hearth) are absorbed
/// by the tier picker (`best_*_for`); we record_tech_use on the tier
/// chosen at completion (see `gating_techs_for_completed_blueprint`).
pub fn build_kind_required_techs(kind: BuildSiteKind) -> Vec<TechId> {
    let mut out = Vec::new();
    if let Some(t) = recipe_for(kind).tech_gate {
        out.push(t);
    }
    out
}

/// Whether a poster carrying `learned` knows enough to author a build of
/// `kind`. Checks the recipe `tech_gate` only — tier picks happen later
/// via `best_*_for(design_techs)` and naturally fall back to the lowest
/// tier the poster knows.
pub fn poster_can_post_kind(kind: BuildSiteKind, learned: &FactionTechs) -> bool {
    build_kind_required_techs(kind)
        .iter()
        .all(|t| learned.has(*t))
}

/// Techs whose successful exercise this completed blueprint represents.
/// Used by `construction_system`'s finalize path to call `record_tech_use`
/// so practice diffuses adoption. Includes the recipe gate plus any
/// tier-driving tech that the chosen tier itself required.
pub fn gating_techs_for_completed_blueprint(bp: &Blueprint) -> Vec<TechId> {
    let mut out = build_kind_required_techs(bp.kind);
    let techs = &bp.design_techs;
    // Tier-driving techs the poster's design relied on. We resolve the
    // chosen tier from `design_techs` and credit the tech that unlocked it.
    match bp.kind {
        BuildSiteKind::Wall(_) => {
            // Wall variants are pre-resolved to a `WallMaterial` at intent
            // time; credit the unlocking tech for that material.
            if let BuildSiteKind::Wall(mat) = bp.kind {
                match mat {
                    WallMaterial::CutStone => out.push(MONUMENTAL_BUILDING),
                    WallMaterial::Stone => out.push(COPPER_WORKING),
                    WallMaterial::Mudbrick => out.push(FIRED_POTTERY),
                    WallMaterial::WattleDaub => out.push(PERM_SETTLEMENT),
                    WallMaterial::Palisade => {}
                }
            }
        }
        BuildSiteKind::Bed => match best_bed_for(techs) {
            BedTier::Carved => out.push(COPPER_TOOLS),
            BedTier::Framed => out.push(FLINT_KNAPPING),
            BedTier::Crude => {}
        },
        BuildSiteKind::Door => match best_door_for(techs) {
            DoorTier::Reinforced => out.push(BRONZE_TOOLS),
            DoorTier::Plank => out.push(FLINT_KNAPPING),
            DoorTier::Wood => {}
        },
        BuildSiteKind::Campfire => match best_hearth_for(techs) {
            HearthTier::Lined => out.push(FIRED_POTTERY),
            HearthTier::Ringed => out.push(FLINT_KNAPPING),
            HearthTier::Open => {}
        },
        BuildSiteKind::Workbench => match best_workbench_for(techs) {
            WorkbenchTier::Bronze => out.push(BRONZE_CASTING),
            WorkbenchTier::Copper => out.push(COPPER_WORKING),
            WorkbenchTier::Stone => {}
        },
        _ => {}
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// Whether a poster carrying `learned` can author *every* gated part of
/// `intent`. Composite intents (Hut/Longhouse/CompositeHouse) require one
/// poster covering all pieces — no Frankenstein mixing of two architects'
/// techs (Improvement #3 / plan strength bullet 3).
pub fn poster_can_post_intent(intent: BuildIntent, learned: &FactionTechs) -> bool {
    intent
        .required_kinds()
        .iter()
        .all(|&k| poster_can_post_kind(k, learned))
}

/// Every tech that gates *some* construction: each build recipe's
/// `tech_gate` plus the tier-driving techs (wall material / bed / door /
/// hearth / workbench ladders). Used by `chief_architect_appointment_system`
/// to decide whether a settlement needs an architect — it does only when a
/// resident knows a construction tech the chief hasn't personally Learned.
pub fn construction_relevant_techs() -> Vec<TechId> {
    let mut out: Vec<TechId> = build_recipes().iter().filter_map(|r| r.tech_gate).collect();
    // Tier-driving techs absorbed by the `best_*_for` ladders (not a recipe
    // `tech_gate`, but still construction knowledge a chief may lack).
    out.extend_from_slice(&[
        PERM_SETTLEMENT,
        FIRED_POTTERY,
        COPPER_WORKING,
        MONUMENTAL_BUILDING,
        FLINT_KNAPPING,
        COPPER_TOOLS,
        BRONZE_TOOLS,
        BRONZE_CASTING,
    ]);
    out.sort_unstable();
    out.dedup();
    out
}

// ── Construction poster pool (sleepy-dove Phase 2) ────────────────────────────

/// Which authority class a resolved poster belongs to. Mirrors
/// `jobs::PosterClass` but scoped to the two construction-relevant classes
/// so `JobPosting.poster_class` can be stamped from a resolved capability.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConstructionPosterClass {
    Chief,
    Architect,
}

impl ConstructionPosterClass {
    pub fn to_job_poster_class(self) -> crate::simulation::jobs::PosterClass {
        match self {
            ConstructionPosterClass::Chief => crate::simulation::jobs::PosterClass::Chief,
            ConstructionPosterClass::Architect => crate::simulation::jobs::PosterClass::Architect,
        }
    }
}

/// A single resolved construction authority: who they are, where they
/// reside, and the `Learned` snapshot they would stamp into a blueprint's
/// `design_techs`. Refreshed read-only every `ParallelA` tick.
#[derive(Clone, Debug)]
pub struct PosterCapability {
    pub entity: Entity,
    pub faction_id: u32,
    pub settlement_id: Option<crate::simulation::settlement::SettlementId>,
    pub learned: FactionTechs,
    pub building_skill: u32,
    pub social_skill: u32,
    pub class: ConstructionPosterClass,
}

impl PosterCapability {
    /// The `BlueprintAuthor` snapshot to stamp onto every blueprint this
    /// poster authorises. Freezes tier picks at intent-spawn time.
    pub fn author(&self) -> BlueprintAuthor {
        BlueprintAuthor::new(self.entity, self.learned)
    }
}

/// Settlement-scoped construction authority index. Replaces the
/// faction-wide `community_adoption_bitset` gate for runtime construction:
/// a band can build whatever any single resident chief or architect has
/// personally **Learned**, regardless of community adoption stage.
///
/// Refreshed read-only by `refresh_construction_poster_pool_system` in
/// `SimulationSet::ParallelA` so construction planning (ParallelB/Economy)
/// never queries `PersonKnowledge` per-tick.
#[derive(Resource, Default)]
pub struct ConstructionPosterPool {
    /// Per-settlement resident posters (chief if resident + architects).
    pub by_settlement:
        AHashMap<(u32, crate::simulation::settlement::SettlementId), Vec<PosterCapability>>,
    /// Faction chief fallback for factions whose chief isn't pinned to a
    /// specific settlement (single-settlement factions, camps).
    pub chief_by_faction: AHashMap<u32, PosterCapability>,
}

impl ConstructionPosterPool {
    /// Union of every resident poster's `Learned` set for the given
    /// settlement, plus the faction chief fallback. This is the
    /// candidate-*enumeration* surface — what `generate_candidates` is
    /// allowed to consider building. Actual emission is still filtered
    /// per-intent through `select_poster_for_intent`.
    pub fn union_of_learned(
        &self,
        faction_id: u32,
        settlement_id: Option<crate::simulation::settlement::SettlementId>,
    ) -> FactionTechs {
        let mut acc = FactionTechs::default();
        if let Some(chief) = self.chief_by_faction.get(&faction_id) {
            acc = acc.union(&chief.learned);
        }
        if let Some(sid) = settlement_id {
            if let Some(list) = self.by_settlement.get(&(faction_id, sid)) {
                for cap in list {
                    acc = acc.union(&cap.learned);
                }
            }
        }
        acc
    }

    /// Convenience wrapper: resolve a poster for a single-tile build
    /// (`BuildIntent::Single`). Used by manual player construction
    /// (`PlayerCommand::Build`) and the right-click menu lock state.
    pub fn select_poster_for_kind(
        &self,
        faction_id: u32,
        settlement_id: Option<crate::simulation::settlement::SettlementId>,
        kind: BuildSiteKind,
    ) -> Option<&PosterCapability> {
        self.select_poster_for_intent(faction_id, settlement_id, BuildIntent::Single(kind))
    }

    /// Resolve the best poster able to author every gated part of
    /// `intent` for this settlement. Chief preferred (broadest authority);
    /// else the resident architect with the widest tech coverage, then
    /// Building skill, Social skill, entity id (deterministic).
    pub fn select_poster_for_intent(
        &self,
        faction_id: u32,
        settlement_id: Option<crate::simulation::settlement::SettlementId>,
        intent: BuildIntent,
    ) -> Option<&PosterCapability> {
        if let Some(chief) = self.chief_by_faction.get(&faction_id) {
            if poster_can_post_intent(intent, &chief.learned) {
                return Some(chief);
            }
        }
        let sid = settlement_id?;
        let list = self.by_settlement.get(&(faction_id, sid))?;
        list.iter()
            .filter(|c| poster_can_post_intent(intent, &c.learned))
            .max_by(|a, b| {
                a.learned
                    .0
                    .count_ones()
                    .cmp(&b.learned.0.count_ones())
                    .then(a.building_skill.cmp(&b.building_skill))
                    .then(a.social_skill.cmp(&b.social_skill))
                    .then(b.entity.cmp(&a.entity))
            })
    }
}

/// Rebuild `ConstructionPosterPool` **and** write each faction's
/// `buildable_techs` — the one construction-tech surface the whole game
/// reads (`community_has` / `community_adoption_bitset`). Nothing gates
/// on community *adoption* any more; this is the single consistent
/// system. Runs in `ParallelA` (and once at `OnEnter` before seeding so
/// the surface is populated before any gate observes it).
///
/// Chief is resolved by `faction.chief_entity` (not the `FactionChief`
/// marker, which `chief_selection_system` only sets on an Economy
/// cadence — resolving by id keeps the surface correct at tick 0).
/// Architects reside at the nearest same-faction settlement; the
/// faction-wide `buildable_techs` is the union of the chief + every
/// resident architect, so a band builds whatever any single resident
/// authority has personally Learned.
pub fn refresh_construction_poster_pool_system(
    mut pool: ResMut<ConstructionPosterPool>,
    mut registry: ResMut<FactionRegistry>,
    settlement_map: Res<crate::simulation::settlement::SettlementMap>,
    settlement_q: Query<&crate::simulation::settlement::Settlement>,
    person_q: Query<(
        Entity,
        &FactionMember,
        &Profession,
        &crate::simulation::knowledge::PersonKnowledge,
        &Skills,
        &Transform,
    )>,
) {
    pool.by_settlement.clear();
    pool.chief_by_faction.clear();

    // Index every person once so the chief can be resolved by entity id
    // regardless of whether the `FactionChief` marker has been stamped.
    let mut by_entity: AHashMap<Entity, (FactionTechs, u32, u32, (i32, i32))> = AHashMap::new();
    let mut architects: Vec<(Entity, u32, FactionTechs, u32, u32, (i32, i32))> = Vec::new();
    for (entity, member, prof, knowledge, skills, xf) in person_q.iter() {
        if member.faction_id == SOLO {
            continue;
        }
        let learned = knowledge.learned_bitset();
        let building = skills.0[SkillKind::Building as usize];
        let social = skills.0[SkillKind::Social as usize];
        let tile = crate::world::terrain::world_to_tile(xf.translation.truncate());
        by_entity.insert(entity, (learned, building, social, tile));
        if *prof == Profession::Architect {
            architects.push((entity, member.faction_id, learned, building, social, tile));
        }
    }

    // Per-faction accumulated buildable surface (chief ∪ all architects).
    let mut union_by_faction: AHashMap<u32, FactionTechs> = AHashMap::new();

    // Chief capability + fallback.
    for (&faction_id, faction) in registry.factions.iter() {
        if faction_id == SOLO {
            continue;
        }
        let Some(chief) = faction.chief_entity else {
            continue;
        };
        let Some(&(learned, building, social, _tile)) = by_entity.get(&chief) else {
            continue;
        };
        let sid = settlement_map.first_for_faction(faction_id);
        let cap = PosterCapability {
            entity: chief,
            faction_id,
            settlement_id: sid,
            learned,
            building_skill: building,
            social_skill: social,
            class: ConstructionPosterClass::Chief,
        };
        if let Some(sid) = sid {
            pool.by_settlement
                .entry((faction_id, sid))
                .or_default()
                .push(cap.clone());
        }
        pool.chief_by_faction.insert(faction_id, cap);
        let acc = union_by_faction.entry(faction_id).or_default();
        *acc = acc.union(&learned);
    }

    // Architects: resident at the nearest same-faction settlement.
    for (entity, faction_id, learned, building, social, tile) in architects {
        let mut best: Option<(crate::simulation::settlement::SettlementId, i32)> = None;
        for &sid in settlement_map.for_faction(faction_id) {
            let Some(&se) = settlement_map.by_id.get(&sid) else {
                continue;
            };
            let Ok(s) = settlement_q.get(se) else {
                continue;
            };
            let d = (s.market_tile.0 - tile.0)
                .abs()
                .max((s.market_tile.1 - tile.1).abs());
            if best.map(|(_, bd)| d < bd).unwrap_or(true) {
                best = Some((sid, d));
            }
        }
        if let Some((sid, _)) = best {
            pool.by_settlement
                .entry((faction_id, sid))
                .or_default()
                .push(PosterCapability {
                    entity,
                    faction_id,
                    settlement_id: Some(sid),
                    learned,
                    building_skill: building,
                    social_skill: social,
                    class: ConstructionPosterClass::Architect,
                });
        }
        // Architect knowledge counts toward the faction surface even
        // before a settlement exists to pin them to.
        let acc = union_by_faction.entry(faction_id).or_default();
        *acc = acc.union(&learned);
    }

    // Write the one construction-tech surface every gate reads.
    for (&faction_id, faction) in registry.factions.iter_mut() {
        faction.buildable_techs = union_by_faction
            .get(&faction_id)
            .copied()
            .unwrap_or_default();
    }
}

/// Returns the recipe for a given build site kind from the lazy table.
pub fn recipe_for(kind: BuildSiteKind) -> &'static BuildRecipe {
    let idx = match kind {
        BuildSiteKind::Wall(WallMaterial::Palisade) => BuildRecipeIdx::Palisade,
        BuildSiteKind::Wall(WallMaterial::WattleDaub) => BuildRecipeIdx::WattleDaub,
        BuildSiteKind::Wall(WallMaterial::Stone) => BuildRecipeIdx::StoneWall,
        BuildSiteKind::Wall(WallMaterial::Mudbrick) => BuildRecipeIdx::Mudbrick,
        BuildSiteKind::Wall(WallMaterial::CutStone) => BuildRecipeIdx::CutStone,
        BuildSiteKind::Door => BuildRecipeIdx::Door,
        BuildSiteKind::Bed => BuildRecipeIdx::Bed,
        BuildSiteKind::Bedroll => BuildRecipeIdx::Bedroll,
        BuildSiteKind::Tent => BuildRecipeIdx::Tent,
        BuildSiteKind::Yurt => BuildRecipeIdx::Yurt,
        BuildSiteKind::Campfire => BuildRecipeIdx::Campfire,
        BuildSiteKind::Workbench => BuildRecipeIdx::Workbench,
        BuildSiteKind::Loom => BuildRecipeIdx::Loom,
        BuildSiteKind::Table => BuildRecipeIdx::Table,
        BuildSiteKind::Chair => BuildRecipeIdx::Chair,
        BuildSiteKind::Granary => BuildRecipeIdx::Granary,
        BuildSiteKind::Shrine => BuildRecipeIdx::Shrine,
        BuildSiteKind::Market => BuildRecipeIdx::Market,
        BuildSiteKind::Barracks => BuildRecipeIdx::Barracks,
        BuildSiteKind::Monument => BuildRecipeIdx::Monument,
        BuildSiteKind::Latrine => BuildRecipeIdx::Latrine,
        BuildSiteKind::Bridge => BuildRecipeIdx::Bridge,
        BuildSiteKind::Dam => BuildRecipeIdx::Dam,
        BuildSiteKind::Well => BuildRecipeIdx::Well,
    };
    &build_recipes()[idx as usize]
}

/// Spiral search outward from `(tx, ty)` for the nearest tile that is
/// passable + non-water-like. Used by Bridge deconstruct so refunds drop on
/// solid ground, not the restored River tile (where they'd be unreachable).
/// Returns `None` only if the chunk_map has no passable land within the cap.
pub fn nearest_passable_bank(chunk_map: &ChunkMap, origin: (i32, i32)) -> Option<(i32, i32)> {
    const MAX_RADIUS: i32 = 8;
    for r in 1..=MAX_RADIUS {
        for dx in -r..=r {
            for dy in -r..=r {
                // Only walk the ring at chebyshev distance r.
                if dx.abs().max(dy.abs()) != r {
                    continue;
                }
                let nx = origin.0 + dx;
                let ny = origin.1 + dy;
                if let Some(kind) = chunk_map.tile_kind_at(nx, ny) {
                    if kind.is_passable() && !kind.is_water_like() {
                        return Some((nx, ny));
                    }
                }
            }
        }
    }
    None
}

/// Adjacent passable, non-Bridge-blueprint cell for a water-anchored
/// blueprint. Workers stand here while building / depositing. Returns the
/// best cardinal first, then diagonals; `None` if every neighbour is sealed.
pub fn work_stand_for_bridge(
    chunk_map: &ChunkMap,
    blueprint_tile: (i32, i32),
    bp_map: &BlueprintMap,
) -> Option<(i32, i32)> {
    // Cardinals first, then diagonals — workers prefer not to take a
    // diagonal step onto the bank.
    const NEIGHBORS: [(i32, i32); 8] = [
        (1, 0),
        (-1, 0),
        (0, 1),
        (0, -1),
        (1, 1),
        (1, -1),
        (-1, 1),
        (-1, -1),
    ];
    for (dx, dy) in NEIGHBORS {
        let nx = blueprint_tile.0 + dx;
        let ny = blueprint_tile.1 + dy;
        if bp_map.0.contains_key(&(nx, ny)) {
            // Another blueprint (Bridge or otherwise) — skip; the next stand
            // candidate is preferred. Two adjacent Bridge blueprints don't
            // both claim each other.
            continue;
        }
        if let Some(kind) = chunk_map.tile_kind_at(nx, ny) {
            if kind.is_passable() && !kind.is_water_like() {
                return Some((nx, ny));
            }
        }
    }
    None
}

/// Count how many of the 4 cardinal directions have a wall (or higher-z terrain)
/// within 3 tiles. Score range: 0–4.
pub fn enclosure_score(chunk_map: &ChunkMap, tx: i32, ty: i32) -> u8 {
    // Enclosure compares solid terrain heights (cliffs/walls), not water.
    let agent_z = chunk_map.ground_z_at(tx, ty);
    let mut score = 0u8;
    for (dx, dy) in [(-1i32, 0), (1, 0), (0, -1i32), (0, 1)] {
        for step in 1..=3i32 {
            let nx = tx + dx * step;
            let ny = ty + dy * step;
            let kind_wall = chunk_map.tile_kind_at(nx, ny) == Some(TileKind::Wall);
            let z_higher = chunk_map.ground_z_at(nx, ny) > agent_z;
            if kind_wall || z_higher {
                score += 1;
                break;
            }
        }
    }
    score
}

// ── Placement helpers ─────────────────────────────────────────────────────────

fn count_beds_near(bed_map: &BedMap, home: (i32, i32), radius: i32) -> usize {
    let (hx, hy) = (home.0 as i32, home.1 as i32);
    bed_map
        .0
        .keys()
        .filter(|&&pos| (pos.0 as i32 - hx).abs() <= radius && (pos.1 as i32 - hy).abs() <= radius)
        .count()
}

fn count_campfires_near(campfire_map: &CampfireMap, home: (i32, i32), radius: i32) -> usize {
    let (hx, hy) = (home.0 as i32, home.1 as i32);
    campfire_map
        .0
        .keys()
        .filter(|&&pos| (pos.0 as i32 - hx).abs() <= radius && (pos.1 as i32 - hy).abs() <= radius)
        .count()
}

fn count_walls_near(wall_map: &WallMap, home: (i32, i32), radius: i32) -> usize {
    let (hx, hy) = (home.0 as i32, home.1 as i32);
    wall_map
        .0
        .keys()
        .filter(|&&pos| (pos.0 as i32 - hx).abs() <= radius && (pos.1 as i32 - hy).abs() <= radius)
        .count()
}

fn count_workbenches_near(map: &WorkbenchMap, home: (i32, i32), radius: i32) -> usize {
    let (hx, hy) = (home.0 as i32, home.1 as i32);
    map.0
        .keys()
        .filter(|&&pos| (pos.0 as i32 - hx).abs() <= radius && (pos.1 as i32 - hy).abs() <= radius)
        .count()
}

fn count_granaries_near(map: &GranaryMap, home: (i32, i32), radius: i32) -> usize {
    let (hx, hy) = (home.0 as i32, home.1 as i32);
    map.0
        .keys()
        .filter(|&&pos| (pos.0 as i32 - hx).abs() <= radius && (pos.1 as i32 - hy).abs() <= radius)
        .count()
}

fn count_wells_near(map: &WellMap, home: (i32, i32), radius: i32) -> usize {
    let (hx, hy) = (home.0 as i32, home.1 as i32);
    map.0
        .keys()
        .filter(|&&pos| (pos.0 as i32 - hx).abs() <= radius && (pos.1 as i32 - hy).abs() <= radius)
        .count()
}

fn count_shrines_near(map: &ShrineMap, home: (i32, i32), radius: i32) -> usize {
    let (hx, hy) = (home.0 as i32, home.1 as i32);
    map.0
        .keys()
        .filter(|&&pos| (pos.0 as i32 - hx).abs() <= radius && (pos.1 as i32 - hy).abs() <= radius)
        .count()
}

fn count_markets_near(map: &MarketMap, home: (i32, i32), radius: i32) -> usize {
    let (hx, hy) = (home.0 as i32, home.1 as i32);
    map.0
        .keys()
        .filter(|&&pos| (pos.0 as i32 - hx).abs() <= radius && (pos.1 as i32 - hy).abs() <= radius)
        .count()
}

fn count_barracks_near(map: &BarracksMap, home: (i32, i32), radius: i32) -> usize {
    let (hx, hy) = (home.0 as i32, home.1 as i32);
    map.0
        .keys()
        .filter(|&&pos| (pos.0 as i32 - hx).abs() <= radius && (pos.1 as i32 - hy).abs() <= radius)
        .count()
}

fn count_monuments_near(map: &MonumentMap, home: (i32, i32), radius: i32) -> usize {
    let (hx, hy) = (home.0 as i32, home.1 as i32);
    map.0
        .keys()
        .filter(|&&pos| (pos.0 as i32 - hx).abs() <= radius && (pos.1 as i32 - hy).abs() <= radius)
        .count()
}

/// Find a clear (2·hw+1) × (2·hh+1) footprint anchored at the frontage edge
/// of a vacant lot of `kind` owned by `faction_id`. Walks every plot of the
/// matching zone kind that has frontage info, picks the first vacant one
/// closest to `home`, and searches inward from its `access_tile` for a clear,
/// flat footprint. Returns the centre.
///
/// "Vacant" here means no Bed and no Blueprint already occupies the plot's
/// rect — sufficient for the chief's one-building-per-residential-lot model.
/// Returns `None` if no plot with frontage exists or none accept a footprint;
/// callers fall back to `find_footprint_in_zone`.
///
/// Phase 3 of the Construction Overhaul: residential placement aligns to
/// streets so doors face roads. Civic placement keeps zone-area scoring.
fn find_footprint_at_frontage_lot(
    chunk_map: &ChunkMap,
    bed_map: &BedMap,
    bp_map: &BlueprintMap,
    doormat: &crate::simulation::doormat::DoormatReservations,
    plot_index: &crate::simulation::land::PlotIndex,
    plot_q: &Query<&crate::simulation::land::Plot>,
    faction_id: u32,
    kind: crate::simulation::settlement::ZoneKind,
    home: (i32, i32),
    half_w: i32,
    half_h: i32,
) -> Option<((i32, i32), crate::simulation::land::TileEdge)> {
    use crate::simulation::land::TileEdge;

    // Gather candidate plots: matching faction + zone kind + has frontage +
    // vacant rect. Sort by chebyshev distance to home so we fill near-home
    // lots first.
    let mut plots: Vec<(
        i32,
        (i32, i32),
        TileEdge,
        crate::simulation::settlement::TileRect,
    )> = Vec::new();
    for (&pid, &entity) in plot_index.by_id.iter() {
        let _ = pid;
        let Ok(plot) = plot_q.get(entity) else {
            continue;
        };
        if plot.faction_id != faction_id || plot.zone_kind != kind {
            continue;
        }
        let (Some(edge), Some(at)) = (plot.frontage_edge, plot.access_tile) else {
            continue;
        };
        if !plot_rect_vacant(bed_map, bp_map, doormat, plot.rect) {
            continue;
        }
        let d = (at.0 - home.0).abs().max((at.1 - home.1).abs());
        plots.push((d, at, edge, plot.rect));
    }
    plots.sort_by_key(|(d, _, _, _)| *d);

    for (_, access, edge, rect) in plots {
        // Anchor centre near the frontage. For East frontage the door faces
        // east; place the centre `half_w + 1` tiles inside the eastern edge.
        let (ax_min, ax_max, ay_min, ay_max) = (
            rect.x0 + half_w,
            rect.x0 + rect.w as i32 - half_w - 1,
            rect.y0 + half_h,
            rect.y0 + rect.h as i32 - half_h - 1,
        );
        if ax_min > ax_max || ay_min > ay_max {
            continue;
        }
        let preferred = match edge {
            TileEdge::East => (ax_max, access.1.clamp(ay_min, ay_max)),
            TileEdge::West => (ax_min, access.1.clamp(ay_min, ay_max)),
            TileEdge::North => (access.0.clamp(ax_min, ax_max), ay_max),
            TileEdge::South => (access.0.clamp(ax_min, ax_max), ay_min),
        };
        // Spiral outward from `preferred` within the plot rect; pick the
        // first clear, low-spread tile.
        let mut best: Option<(u8, i32, (i32, i32))> = None;
        for cy in ay_min..=ay_max {
            for cx in ax_min..=ax_max {
                if !is_clear_footprint(chunk_map, bed_map, bp_map, doormat, cx, cy, half_w, half_h)
                {
                    continue;
                }
                let (_, spread) = footprint_z_stats(chunk_map, cx, cy, half_w, half_h);
                if spread > MAX_TERRAFORM_SPREAD {
                    continue;
                }
                let d = (cx - preferred.0).abs() + (cy - preferred.1).abs();
                let cand = (spread, d, (cx, cy));
                if best.map(|b| (cand.0, cand.1) < (b.0, b.1)).unwrap_or(true) {
                    best = Some(cand);
                }
            }
        }
        if let Some((_, _, p)) = best {
            return Some((p, edge));
        }
    }
    None
}

/// True iff no Bed and no Blueprint sits inside `rect`. Cheap rect-scan;
/// chief residential lots are at most 6×6 = 36 tiles.
fn plot_rect_vacant(
    bed_map: &BedMap,
    bp_map: &BlueprintMap,
    doormat: &crate::simulation::doormat::DoormatReservations,
    rect: crate::simulation::settlement::TileRect,
) -> bool {
    for ty in rect.y0..rect.y0 + rect.h as i32 {
        for tx in rect.x0..rect.x0 + rect.w as i32 {
            if bed_map.0.contains_key(&(tx, ty)) || bp_map.0.contains_key(&(tx, ty)) {
                return false;
            }
            if doormat.is_reserved((tx, ty)) {
                return false;
            }
        }
    }
    true
}

/// Find a clear (2·hw+1) × (2·hh+1) footprint inside the first matching zone
/// of `plan`. Returns the centre. Falls back to `find_building_origin` (radial
/// search around home) when no matching zone exists or the zone is full.
fn find_footprint_in_zone(
    chunk_map: &ChunkMap,
    bed_map: &BedMap,
    bp_map: &BlueprintMap,
    doormat: &crate::simulation::doormat::DoormatReservations,
    plan: Option<&crate::simulation::settlement::SettlementPlan>,
    kind: crate::simulation::settlement::ZoneKind,
    home: (i32, i32),
    half_w: i32,
    half_h: i32,
    fallback_radius: i32,
) -> Option<(i32, i32)> {
    let (hx, hy) = (home.0 as i32, home.1 as i32);

    if let Some(plan) = plan {
        if let Some(rect) = plan.zones.iter().find(|z| z.kind == kind).map(|z| z.rect) {
            // Rank candidates by (spread asc, distance asc). Flat ground is
            // strongly preferred; uneven sites that exceed MAX_TERRAFORM_SPREAD
            // are rejected outright so we don't queue megaprojects.
            let mut best: Option<(u8, i32, (i32, i32))> = None;
            let cx_min = rect.x0 as i32 + half_w;
            let cy_min = rect.y0 as i32 + half_h;
            let cx_max = rect.x0 as i32 + rect.w as i32 - half_w - 1;
            let cy_max = rect.y0 as i32 + rect.h as i32 - half_h - 1;
            for cy in cy_min..=cy_max {
                for cx in cx_min..=cx_max {
                    if !is_clear_footprint(
                        chunk_map, bed_map, bp_map, doormat, cx, cy, half_w, half_h,
                    ) {
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
                    let cand = (spread, d, (cx as i32, cy as i32));
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
        doormat,
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
    doormat: &crate::simulation::doormat::DoormatReservations,
    plan: Option<&crate::simulation::settlement::SettlementPlan>,
    kind: crate::simulation::settlement::ZoneKind,
    home: (i32, i32),
    fallback_radius: i32,
) -> Option<(i32, i32)> {
    let (hx, hy) = (home.0 as i32, home.1 as i32);

    if let Some(plan) = plan {
        if let Some(rect) = plan.zones.iter().find(|z| z.kind == kind).map(|z| z.rect) {
            let mut best: Option<(i32, (i32, i32))> = None;
            for dy in 0..rect.h as i32 {
                for dx in 0..rect.w as i32 {
                    let tx = rect.x0 as i32 + dx;
                    let ty = rect.y0 as i32 + dy;
                    let pos = (tx as i32, ty as i32);
                    if bp_map.0.contains_key(&pos) || bed_map.0.contains_key(&pos) {
                        continue;
                    }
                    if doormat.is_reserved(pos) {
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
                let pos = ((hx + dx) as i32, (hy + dy) as i32);
                if bp_map.0.contains_key(&pos) || bed_map.0.contains_key(&pos) {
                    continue;
                }
                if doormat.is_reserved(pos) {
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
    doormat: &crate::simulation::doormat::DoormatReservations,
    campfire_map: &CampfireMap,
    plan: Option<&crate::simulation::settlement::SettlementPlan>,
    home: (i32, i32),
) -> Option<(i32, i32)> {
    use crate::simulation::settlement::ZoneKind;
    if let Some(plan) = plan {
        for zone in plan.zones.iter().filter(|z| z.kind == ZoneKind::Civic) {
            let rect = zone.rect;
            let occupied = campfire_map.0.keys().any(|&(x, y)| rect.contains(x, y));
            if occupied {
                continue;
            }
            let mut best: Option<(i32, (i32, i32))> = None;
            let zcx = rect.x0 as i32 + rect.w as i32 / 2;
            let zcy = rect.y0 as i32 + rect.h as i32 / 2;
            for dy in 0..rect.h as i32 {
                for dx in 0..rect.w as i32 {
                    let tx = rect.x0 as i32 + dx;
                    let ty = rect.y0 as i32 + dy;
                    let pos = (tx as i32, ty as i32);
                    if bp_map.0.contains_key(&pos) || bed_map.0.contains_key(&pos) {
                        continue;
                    }
                    if doormat.is_reserved(pos) {
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
                let pos = ((hx + dx) as i32, (hy + dy) as i32);
                if bp_map.0.contains_key(&pos)
                    || bed_map.0.contains_key(&pos)
                    || campfire_map.0.contains_key(&pos)
                {
                    continue;
                }
                if doormat.is_reserved(pos) {
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
    doormat: &crate::simulation::doormat::DoormatReservations,
    hearths: &[(i32, i32)],
    home: (i32, i32),
    inner_r: i32,
    outer_r: i32,
) -> Option<(i32, i32)> {
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

    let mut best: Option<(i32, (i32, i32))> = None;
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
                let pos = (tx as i32, ty as i32);
                if bp_map.0.contains_key(&pos) || bed_map.0.contains_key(&pos) {
                    continue;
                }
                if doormat.is_reserved(pos) {
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
            // Parcel flatness = solid-ground spread (bed, not water surface).
            let z = chunk_map.ground_z_at(cx + dx, cy + dy);
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
    doormat: &crate::simulation::doormat::DoormatReservations,
    cx: i32,
    cy: i32,
    half_w: i32,
    half_h: i32,
) -> bool {
    for dy in -half_h..=half_h {
        for dx in -half_w..=half_w {
            let pos = ((cx + dx) as i32, (cy + dy) as i32);
            if bp_map.0.contains_key(&pos) {
                return false;
            }
            if bed_map.0.contains_key(&pos) {
                return false;
            }
            if doormat.is_reserved(pos) {
                return false;
            }
            let Some(kind) = chunk_map.tile_kind_at(cx + dx, cy + dy) else {
                return false;
            };
            // Buildings never land on existing Roads — main thoroughfares
            // must stay open. Without this gate a hut can plant its wall
            // straight across a carved street, severing the network.
            if !kind.is_passable() || kind == TileKind::Wall || kind == TileKind::Road {
                return false;
            }
        }
    }
    true
}

/// Step 7: emergency-shelter bed placement when every wall ladder rung is
/// unobtainable (`select_wall_material → EmergencyShelter`). Picks a single
/// clear tile on a deterministic outward sweep through an **era-keyed
/// annulus** around `home` — Neolithic flings emergency beds to the
/// outskirts/slum fringe, Chalcolithic packs work-yard bunk rows mid-ring,
/// Bronze packs civic-overflow rows nearer the core. One era-parameterised
/// finder rather than three near-duplicates (the annulus *is* the era
/// distinction). Reuses `is_clear_footprint` (1×1) so it rejects roads,
/// walls, water, blueprints, beds, and doormats exactly like every other
/// placement helper. Determinism: the angular phase is seeded from the
/// faction layout seed XOR `bed_count`, so successive emergency beds form a
/// loose row instead of stacking on one bearing.
pub(crate) fn find_emergency_bed_tile(
    chunk_map: &ChunkMap,
    bed_map: &BedMap,
    bp_map: &BlueprintMap,
    doormat: &crate::simulation::doormat::DoormatReservations,
    home: (i32, i32),
    era: Era,
    layout_seed: u64,
    bed_count: i32,
) -> Option<(i32, i32)> {
    let (inner, outer) = match era {
        Era::Neolithic => (12, 32),   // outskirts / slum rows
        Era::Chalcolithic => (8, 22), // work-yard bunk rows
        _ => (6, 18),                 // Bronze: civic-overflow rows
    };
    let mut rng =
        fastrand::Rng::with_seed(layout_seed ^ (bed_count as u64).wrapping_mul(0x9E37_79B9));
    let base_ang = rng.f32() * std::f32::consts::TAU;
    for r in inner..=outer {
        let steps = (r * 6).max(8);
        for s in 0..steps {
            let ang = base_ang + (s as f32 / steps as f32) * std::f32::consts::TAU;
            let tx = home.0 + (ang.cos() * r as f32).round() as i32;
            let ty = home.1 + (ang.sin() * r as f32).round() as i32;
            if is_clear_footprint(chunk_map, bed_map, bp_map, doormat, tx, ty, 0, 0) {
                return Some((tx, ty));
            }
        }
    }
    None
}

/// Shape-aware variant of `is_clear_footprint`: walks every tile in the
/// `shape` mask under `rotation` at `anchor` and rejects when any cell is
/// non-passable, walled, beded, blueprinted, or reserved as another door's
/// doormat. Used by `BuildIntent::CompositeHouse` placement so non-rectangular
/// masks don't drift onto impassable interior tiles that a bounding-box
/// `is_clear_footprint` wouldn't catch.
fn is_clear_shape(
    chunk_map: &ChunkMap,
    bed_map: &BedMap,
    bp_map: &BlueprintMap,
    doormat: &crate::simulation::doormat::DoormatReservations,
    shape: crate::simulation::building_template::FootprintShape,
    rotation: crate::simulation::building_template::Rotation,
    anchor: (i32, i32),
) -> bool {
    for (tx, ty) in crate::simulation::building_template::shape_tiles(shape, anchor, rotation) {
        let pos = (tx, ty);
        if bp_map.0.contains_key(&pos) || bed_map.0.contains_key(&pos) {
            return false;
        }
        if doormat.is_reserved(pos) {
            return false;
        }
        let Some(kind) = chunk_map.tile_kind_at(tx, ty) else {
            return false;
        };
        if !kind.is_passable() || kind == TileKind::Wall || kind == TileKind::Road {
            return false;
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
            if bed_map.0.contains_key(&(nx as i32, ny as i32)) {
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
fn blocks_cardinal_corridor(cx: i32, cy: i32, half_w: i32, half_h: i32, home: (i32, i32)) -> bool {
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
    doormat: &crate::simulation::doormat::DoormatReservations,
    camp_home: (i32, i32),
    half_w: i32,
    half_h: i32,
    max_radius: i32,
) -> Option<(i32, i32)> {
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
                if !is_clear_footprint(chunk_map, bed_map, bp_map, doormat, cx, cy, half_w, half_h)
                {
                    continue;
                }
                if ring <= early_ring {
                    return Some((cx as i32, cy as i32));
                }
                // Beyond the seeding zone, grow organically: require adjacency.
                if has_nearby_structure(chunk_map, bed_map, cx, cy, half_w, half_h, 2) {
                    return Some((cx as i32, cy as i32));
                }
            }
        }
    }
    None
}

/// Phase 1/2: plan all wall and bed blueprints for a single rectangular building.
/// The perimeter wall tile closest to camp_home becomes the entrance (left open).
/// `wall_material` controls which wall recipe is used for every perimeter tile.
/// Pick the perimeter cell on the given cardinal side of a rectangular
/// footprint. Returned as `(dx, dy)` offsets from the centre — guaranteed to
/// be a flat side (not a corner). For even-length sides the cell closest to
/// `camp_home` along the perpendicular axis is chosen so multi-building rows
/// flow naturally toward the village core.
fn entrance_cell_for_edge(
    half_w: i32,
    half_h: i32,
    edge: crate::simulation::land::TileEdge,
    camp_home: (i32, i32),
    centre: (i32, i32),
) -> (i32, i32) {
    use crate::simulation::land::TileEdge;
    // For odd-length sides (half_w == 1 → length 3), the only non-corner
    // cell is dx == 0 (N/S edge) or dy == 0 (E/W edge). For larger sides
    // we clamp the projection of camp_home onto the edge.
    match edge {
        TileEdge::East => (
            half_w,
            ((camp_home.1 - centre.1).clamp(-(half_h - 1).max(0), (half_h - 1).max(0))),
        ),
        TileEdge::West => (
            -half_w,
            ((camp_home.1 - centre.1).clamp(-(half_h - 1).max(0), (half_h - 1).max(0))),
        ),
        TileEdge::North => (
            ((camp_home.0 - centre.0).clamp(-(half_w - 1).max(0), (half_w - 1).max(0))),
            half_h,
        ),
        TileEdge::South => (
            ((camp_home.0 - centre.0).clamp(-(half_w - 1).max(0), (half_w - 1).max(0))),
            -half_h,
        ),
    }
}

/// Canonical wall+door+bed enumeration for a rectangular walled house.
///
/// One source of truth shared by both seed-time direct stamping
/// (`seed_walled_house_at`) and runtime blueprint emission (`plan_building`).
/// Returns tiles in stable order: perimeter cells (walls + door) first in
/// row-major scan order, then interior beds in caller order. The door cell
/// carries `Some(door_edge)`; every other cell carries `None`.
///
/// Note: this is pure layout — it does NOT pick the door cardinal or check
/// for clear ground. Callers run `pick_clear_door_cardinal` first and pass
/// the resolved entrance offset + edge.
pub(crate) fn walled_house_tile_plan(
    cx: i32,
    cy: i32,
    half_w: i32,
    half_h: i32,
    entrance: (i32, i32),
    door_edge: crate::simulation::land::TileEdge,
    wall_material: WallMaterial,
    interior_beds: &[(i32, i32)],
) -> Vec<(
    BuildSiteKind,
    (i32, i32),
    Option<crate::simulation::land::TileEdge>,
)> {
    let mut plan: Vec<(
        BuildSiteKind,
        (i32, i32),
        Option<crate::simulation::land::TileEdge>,
    )> = Vec::new();
    for dy in -half_h..=half_h {
        for dx in -half_w..=half_w {
            if dx.abs() < half_w && dy.abs() < half_h {
                continue; // interior — beds go via the second loop below
            }
            let tile = (cx + dx, cy + dy);
            let (kind, edge) = if (dx, dy) == entrance {
                (BuildSiteKind::Door, Some(door_edge))
            } else {
                (BuildSiteKind::Wall(wall_material), None)
            };
            plan.push((kind, tile, edge));
        }
    }
    for &(bdx, bdy) in interior_beds {
        let tile = (cx + bdx, cy + bdy);
        plan.push((BuildSiteKind::Bed, tile, None));
    }
    plan
}

/// Simulated-build reachability gate over a `walled_house_tile_plan`-style
/// plan: with the finished walls in place, the doormat must connect to `home`
/// and every interior bed must be reachable from the doormat *through the
/// door*. Returns `true` (accept) when the plan has no door (nothing to gate).
/// Shared by `plan_building` and `seed_walled_house_at`.
fn plan_reachable_from_home(
    chunk_map: &ChunkMap,
    home: (i32, i32),
    doormat: (i32, i32),
    plan: &[(
        BuildSiteKind,
        (i32, i32),
        Option<crate::simulation::land::TileEdge>,
    )],
) -> bool {
    let mut walls: AHashSet<(i32, i32)> = AHashSet::new();
    let mut beds: Vec<(i32, i32)> = Vec::new();
    let mut door: Option<(i32, i32)> = None;
    for (kind, tile, _edge) in plan {
        match kind {
            BuildSiteKind::Wall(_) => {
                walls.insert(*tile);
            }
            BuildSiteKind::Bed => beds.push(*tile),
            BuildSiteKind::Door => door = Some(*tile),
            _ => {}
        }
    }
    let Some(door) = door else {
        return true;
    };
    crate::simulation::placement_reachability::simulate_house_reachable(
        chunk_map, home, doormat, door, &walls, &beds,
    )
}

fn plan_building(
    commands: &mut Commands,
    bp_map: &mut BlueprintMap,
    terraform_map: &mut crate::simulation::terraform::TerraformMap,
    pending_footprints: &mut crate::simulation::terraform::PendingFootprints,
    chunk_map: &ChunkMap,
    bed_map: &BedMap,
    doormat_res: &crate::simulation::doormat::DoormatReservations,
    cx: i32,
    cy: i32,
    half_w: i32,
    half_h: i32,
    faction_id: u32,
    camp_home: (i32, i32),
    interior_beds: &[(i32, i32)],
    wall_material: WallMaterial,
    door_dir: Option<crate::simulation::land::TileEdge>,
    author: Option<BlueprintAuthor>,
) {
    // Door direction: prefer the sourced cardinal (plot frontage); fall back
    // to cardinal-toward-home. If that cardinal's doormat is blocked, try
    // other cardinals. Abort the build if none work — placing an unreachable
    // door is strictly worse than placing nothing.
    let preferred_edge =
        door_dir.unwrap_or_else(|| crate::simulation::land::TileEdge::toward((cx, cy), camp_home));
    let Some((door_edge, entrance, planned_doormat)) = pick_clear_door_cardinal(
        chunk_map,
        bed_map,
        bp_map,
        doormat_res,
        (cx, cy),
        half_w,
        half_h,
        preferred_edge,
        camp_home,
    ) else {
        return;
    };

    let (target_z, _spread) = footprint_z_stats(chunk_map, cx, cy, half_w, half_h);

    // Build the wall+door+bed plan. We always compute it first so the deferred
    // path (footprint_completion_system) can spawn the same blueprints once
    // terraform completes. Same enumeration the seed path uses.
    let wall_plan = walled_house_tile_plan(
        cx,
        cy,
        half_w,
        half_h,
        entrance,
        door_edge,
        wall_material,
        interior_beds,
    );

    // Simulated-build reachability gate: validate the house *as it will exist
    // once built* — doormat connects home through the finished walls and every
    // interior bed is reachable from the doormat via the door. Aborting beats
    // shipping a house whose bed is sealed behind its own wall ring.
    if !plan_reachable_from_home(chunk_map, camp_home, planned_doormat, &wall_plan) {
        return;
    }

    // Collect the tiles that need terraforming (footprint covers walls AND
    // interior — every tile under the building must sit at target_z so the
    // floor is level).
    let mut terraform_tiles: Vec<(i32, i32)> = Vec::new();
    for dy in -half_h..=half_h {
        for dx in -half_w..=half_w {
            let tx = cx + dx;
            let ty = cy + dy;
            let surf = chunk_map.surface_z_at(tx, ty);
            if surf as i8 != target_z {
                terraform_tiles.push((tx as i32, ty as i32));
            }
        }
    }

    if terraform_tiles.is_empty() {
        // Flat ground: spawn wall blueprints immediately.
        for (kind, tile, edge) in &wall_plan {
            if bp_map.0.contains_key(tile) {
                continue;
            }
            let wp = tile_to_world(tile.0 as i32, tile.1 as i32);
            let mut bp =
                Blueprint::new(faction_id, None, *kind, *tile, target_z).with_author(author);
            if let Some(e) = edge {
                bp = bp.with_door_dir(*e);
            }
            let e = commands
                .spawn((
                    bp,
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
        author,
    });
}

/// Phase 1/2: find a single open slot on the rectangular palisade that wraps the
/// settlement's bed bounding box plus a buffer. Returns None when the palisade is
/// complete or no beds exist near camp.
fn find_palisade_site(
    chunk_map: &ChunkMap,
    bed_map: &BedMap,
    bp_map: &BlueprintMap,
    doormat: &crate::simulation::doormat::DoormatReservations,
    camp_home: (i32, i32),
    buffer: i32,
) -> Option<(i32, i32)> {
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

    // Top and bottom rows — leave a 3-tile gateway centred on x=hx for each
    // cardinal axis so the spine has real flow capacity instead of a single-
    // tile choke. Same width applied below for E/W columns.
    let gateway_half = 1i32; // half-width: gateway spans [hx-1, hx+1] (3 tiles)
    for x in min_x..=max_x {
        for &y in &[min_y, max_y] {
            if (x - hx).abs() <= gateway_half {
                continue; // N or S gateway: keep open for cardinal access
            }
            let tile = (x as i32, y as i32);
            if bp_map.0.contains_key(&tile) {
                continue;
            }
            if doormat.is_reserved(tile) {
                continue; // never wall over a door's doormat
            }
            let Some(kind) = chunk_map.tile_kind_at(x, y) else {
                continue;
            };
            // Skip Road too — the spine carves Road through the perimeter
            // band; we don't want a palisade segment paving over a street.
            if !kind.is_passable() || kind == TileKind::Wall || kind == TileKind::Road {
                continue;
            }
            return Some(tile);
        }
    }
    // Left and right columns (excluding corners) — 3-tile gateway centred on y=hy.
    for y in (min_y + 1)..max_y {
        for &x in &[min_x, max_x] {
            if (y - hy).abs() <= gateway_half {
                continue; // W or E gateway: keep open for cardinal access
            }
            let tile = (x as i32, y as i32);
            if bp_map.0.contains_key(&tile) {
                continue;
            }
            if doormat.is_reserved(tile) {
                continue;
            }
            let Some(kind) = chunk_map.tile_kind_at(x, y) else {
                continue;
            };
            if !kind.is_passable() || kind == TileKind::Wall || kind == TileKind::Road {
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
    pub well_map: Res<'w, WellMap>,
    pub doormat: Res<'w, crate::simulation::doormat::DoormatReservations>,
    pub organic_selected: Res<'w, crate::simulation::organic_settlement::SelectedSettlementIntents>,
    pub organic_brains: Res<'w, crate::simulation::organic_settlement::SettlementBrains>,
    // sleepy-dove Phase 4: bundled here so `chief_directive_system`
    // stays under Bevy's 16-param ceiling.
    pub poster_pool: Res<'w, ConstructionPosterPool>,
}

/// Read-only borrow of the structure-map set that `generate_candidates` needs.
/// Produced from either `BuildingMapsRO` (chief path) or `FurnitureMaps` (seed
/// path) so the same candidate generator drives both contexts. Doormat /
/// organic_selected / organic_brains stay separate parameters because the
/// chief and seed paths have different access patterns.
pub struct GenCandidatesMaps<'a> {
    pub bed_map: &'a BedMap,
    pub wall_map: &'a WallMap,
    pub campfire_map: &'a CampfireMap,
    pub workbench_map: &'a WorkbenchMap,
    pub granary_map: &'a GranaryMap,
    pub shrine_map: &'a ShrineMap,
    pub market_map: &'a MarketMap,
    pub barracks_map: &'a BarracksMap,
    pub monument_map: &'a MonumentMap,
    pub well_map: &'a WellMap,
}

impl<'w> BuildingMapsRO<'w> {
    pub fn as_view(&self) -> GenCandidatesMaps<'_> {
        GenCandidatesMaps {
            bed_map: &self.bed_map,
            wall_map: &self.wall_map,
            campfire_map: &self.campfire_map,
            workbench_map: &self.workbench_map,
            granary_map: &self.granary_map,
            shrine_map: &self.shrine_map,
            market_map: &self.market_map,
            barracks_map: &self.barracks_map,
            monument_map: &self.monument_map,
            well_map: &self.well_map,
        }
    }
}

impl<'w> FurnitureMaps<'w> {
    pub fn as_view(&self) -> GenCandidatesMaps<'_> {
        GenCandidatesMaps {
            bed_map: &self.bed_map,
            wall_map: &self.wall_map,
            campfire_map: &self.campfire_map,
            workbench_map: &self.workbench_map,
            granary_map: &self.granary_map,
            shrine_map: &self.shrine_map,
            market_map: &self.market_map,
            barracks_map: &self.barracks_map,
            monument_map: &self.monument_map,
            well_map: &self.well_map,
        }
    }
}

/// One thing the chief is considering building this tick. The selector
/// generates several candidates and picks the one with the highest score.
struct BuildCandidate {
    intent: BuildIntent,
    /// Centre tile for the placement (single-tile target or footprint centre).
    tile: (i32, i32),
    score: f32,
    /// Door opening cardinal sourced from the plot's `frontage_edge`. `None`
    /// for civic/zone-area placements that don't have a frontage; the door
    /// then falls back to the cardinal-toward-home rule inside `plan_building`.
    door_dir: Option<crate::simulation::land::TileEdge>,
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
    /// Composite house with an irregular footprint (L-shape, courtyard, …).
    /// Walls span the perimeter of `shape` under `rotation`; one door on
    /// the frontage cardinal; interior tiles take beds (1 bed per cell up
    /// to the shape's interior count). Used by Chalcolithic+ residential.
    CompositeHouse {
        shape: crate::simulation::building_template::FootprintShape,
        rotation: crate::simulation::building_template::Rotation,
        wall_material: WallMaterial,
    },
}

fn build_candidate_from_organic(
    intent: &crate::simulation::organic_settlement::ConstructionIntent,
) -> BuildCandidate {
    let build_intent = match intent.build_kind {
        crate::simulation::organic_settlement::OrganicBuildKind::Single(kind) => {
            BuildIntent::Single(kind)
        }
        crate::simulation::organic_settlement::OrganicBuildKind::Hut(mat) => BuildIntent::Hut(mat),
        crate::simulation::organic_settlement::OrganicBuildKind::Longhouse(mat) => {
            BuildIntent::Longhouse(mat)
        }
        crate::simulation::organic_settlement::OrganicBuildKind::PalisadeSegment(mat) => {
            BuildIntent::PalisadeSegment(mat, 2)
        }
        crate::simulation::organic_settlement::OrganicBuildKind::CompositeHouse {
            shape,
            rotation,
            wall_material,
        } => BuildIntent::CompositeHouse {
            shape,
            rotation,
            wall_material,
        },
    };
    BuildCandidate {
        intent: build_intent,
        tile: intent.tile,
        score: intent.priority,
        door_dir: intent.door_dir,
    }
}

fn candidate_touches_planned_road(
    candidate: &BuildCandidate,
    faction_id: u32,
    settlement_map: &crate::simulation::settlement::SettlementMap,
    brains: &crate::simulation::organic_settlement::SettlementBrains,
) -> bool {
    let Some(sid) = settlement_map.first_for_faction(faction_id) else {
        return false;
    };
    let Some(brain) = brains.0.get(&sid) else {
        return false;
    };
    candidate_footprint_tiles(candidate)
        .into_iter()
        .any(|tile| brain.road_tiles.contains(&tile))
}

fn candidate_footprint_tiles(candidate: &BuildCandidate) -> Vec<(i32, i32)> {
    match candidate.intent {
        BuildIntent::Single(_) | BuildIntent::PalisadeSegment(_, _) => vec![candidate.tile],
        BuildIntent::Hut(_) => rect_tiles(candidate.tile, 1, 1),
        BuildIntent::Longhouse(_) => rect_tiles(candidate.tile, 2, 1),
        BuildIntent::CompositeHouse {
            shape, rotation, ..
        } => crate::simulation::building_template::shape_tiles(shape, candidate.tile, rotation),
    }
}

fn rect_tiles(centre: (i32, i32), half_w: i32, half_h: i32) -> Vec<(i32, i32)> {
    let mut out = Vec::new();
    for dy in -half_h..=half_h {
        for dx in -half_w..=half_w {
            out.push((centre.0 + dx, centre.1 + dy));
        }
    }
    out
}

impl BuildIntent {
    /// The primary input goods this intent will require, summed across every
    /// blueprint it would spawn. Used by the deficit-EMA feedback loop in
    /// `generate_candidates` so candidates that need a chronically-scarce
    /// good get down-scored.
    fn required_goods(self) -> ahash::AHashMap<crate::economy::resource_catalog::ResourceId, u32> {
        let mut totals: ahash::AHashMap<_, u32> = ahash::AHashMap::new();
        let mut add = |kind: BuildSiteKind, multiplier: u32| {
            for &(rid, qty) in &recipe_for(kind).inputs {
                *totals.entry(rid).or_insert(0) += qty as u32 * multiplier;
            }
        };
        match self {
            BuildIntent::Single(kind) => add(kind, 1),
            BuildIntent::Hut(mat) => {
                // 4 wall tiles + 1 door + 1 bed.
                add(BuildSiteKind::Wall(mat), 4);
                add(BuildSiteKind::Door, 1);
                add(BuildSiteKind::Bed, 1);
            }
            BuildIntent::Longhouse(mat) => {
                add(BuildSiteKind::Wall(mat), 8);
                add(BuildSiteKind::Door, 1);
                add(BuildSiteKind::Bed, 2);
            }
            BuildIntent::PalisadeSegment(mat, _) => add(BuildSiteKind::Wall(mat), 1),
            BuildIntent::CompositeHouse {
                shape,
                rotation,
                wall_material,
            } => {
                use crate::simulation::building_template::shape_tiles;
                // Walk canonical tiles, classify perimeter vs interior. We
                // don't know exact wall/bed counts without inspecting the
                // shape mask — close enough is fine for the deficit-EMA
                // feedback loop. Perimeter ≈ outside-facing tiles; interior
                // ≈ everything else, which we map to beds.
                let tiles = shape_tiles(shape, (0, 0), rotation);
                let tile_set: ahash::AHashSet<(i32, i32)> = tiles.iter().copied().collect();
                let mut perim = 0u32;
                let mut interior = 0u32;
                for &(tx, ty) in &tiles {
                    let is_perim = [(1, 0), (-1, 0), (0, 1), (0, -1)]
                        .iter()
                        .any(|&(ox, oy)| !tile_set.contains(&(tx + ox, ty + oy)));
                    if is_perim {
                        perim += 1;
                    } else {
                        interior += 1;
                    }
                }
                add(BuildSiteKind::Wall(wall_material), perim.saturating_sub(1));
                add(BuildSiteKind::Door, 1);
                add(BuildSiteKind::Bed, interior.max(1));
            }
        }
        totals
    }

    /// Every `BuildSiteKind` this intent would emit at least one blueprint
    /// for. Used by the poster pool: a single poster must be able to author
    /// *all* gated parts (no Frankenstein composite mixing two architects'
    /// tech). Tier-flattened wall material is preserved so the recipe gate
    /// (`poster_can_post_kind`) sees the actual material the intent picked.
    pub fn required_kinds(self) -> Vec<BuildSiteKind> {
        let mut out: Vec<BuildSiteKind> = Vec::with_capacity(4);
        let mut push = |k: BuildSiteKind| {
            if !out.contains(&k) {
                out.push(k);
            }
        };
        match self {
            BuildIntent::Single(kind) => push(kind),
            BuildIntent::Hut(mat) | BuildIntent::Longhouse(mat) => {
                push(BuildSiteKind::Wall(mat));
                push(BuildSiteKind::Door);
                push(BuildSiteKind::Bed);
            }
            BuildIntent::PalisadeSegment(mat, _) => push(BuildSiteKind::Wall(mat)),
            BuildIntent::CompositeHouse { wall_material, .. } => {
                push(BuildSiteKind::Wall(wall_material));
                push(BuildSiteKind::Door);
                push(BuildSiteKind::Bed);
            }
        }
        out
    }
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
    // Chief PersonKnowledge powers the sleepy-dove BlueprintAuthor snapshot
    // for runtime intent emission. Replaces the prior unused chief_query
    // (`AgentGoal`-gating was retired — see comment block below).
    chief_knowledge_q: Query<&crate::simulation::knowledge::PersonKnowledge, With<FactionChief>>,
    plot_index: Res<crate::simulation::land::PlotIndex>,
    plot_q: Query<&crate::simulation::land::Plot>,
    settlement_map: Res<crate::simulation::settlement::SettlementMap>,
    settlement_q: Query<&crate::simulation::settlement::Settlement>,
) {
    if clock.tick % 60 != 0 || !auto_build.0 {
        return;
    }
    // sleepy-dove Phase 4: poster pool (bundled in `maps`) replaces the
    // faction-wide community-adoption gate. The settlement's buildable
    // surface is the union of resident chief + architect Learned; each
    // emitted intent is filtered to one poster who can author every part.
    let poster_pool = &maps.poster_pool;

    // Chief AgentGoal::Lead is no longer required — construction queueing reads
    // FactionData and shouldn't pause when the chief eats or sleeps. The chief
    // query was replaced with `chief_knowledge_q` (sleepy-dove): we need the
    // chief's `PersonKnowledge.learned` to snapshot into `BlueprintAuthor`.

    let mut faction_bp_count: AHashMap<u32, usize> = AHashMap::new();
    // Pending-blueprint kind counters per faction. Every civic gate inside
    // `generate_candidates` previously checked *built* counts only, so a
    // structure already queued in `GatherMaterials` didn't satisfy its own
    // gate — every chief tick the same kind got re-queued (visible as "3
    // campfires queued at once" once Phase 1 lifted the one-bp-at-a-time
    // cap). The PendingKindCounts table is consulted alongside the built
    // counts so an in-flight blueprint counts toward fulfillment.
    let mut pending_kinds_per_faction: AHashMap<u32, AHashMap<BuildSiteKind, u32>> =
        AHashMap::new();
    for &bp_entity in bp_map.0.values() {
        if let Ok(bp) = bp_query.get(bp_entity) {
            *faction_bp_count.entry(bp.faction_id).or_insert(0) += 1;
            *pending_kinds_per_faction
                .entry(bp.faction_id)
                .or_default()
                .entry(bp.kind)
                .or_insert(0) += 1;
        }
    }
    // Pending footprints (mid-terraform, no walls yet) also count as
    // in-flight projects so the chief doesn't queue a second building on
    // top of an unfinished levelling job.
    for pending in &pending_footprints.queue {
        *faction_bp_count.entry(pending.faction_id).or_insert(0) += 1;
    }

    let empty_pending: AHashMap<BuildSiteKind, u32> = AHashMap::new();
    for (&faction_id, faction) in faction_registry.factions.iter() {
        if faction_id == SOLO || faction.member_count == 0 {
            continue;
        }
        // Nomadic factions skip the settled chief's build menu entirely:
        // no Hut/Longhouse/Granary/Wall queueing. Their seeded camp from
        // `seed_nomadic_camp` carries them until Phase 8's migration commit
        // re-seeds at the new camp; Phase 7's `nomad_chief_directives` will
        // own replenishment of lost Tents/Bedrolls/Yurts.
        // Capability check: archetypes with no posting layer have no
        // chief allocating construction (today's nomadic behaviour).
        if faction.caps.posting.is_disabled() {
            continue;
        }
        // Phase C: Packed (mobile) bands skip the chief directive.
        if matches!(
            faction.camp_state,
            crate::simulation::faction::CampState::Packed { .. }
        ) {
            continue;
        }
        let count = faction_bp_count.get(&faction_id).copied().unwrap_or(0);
        if count >= MAX_BLUEPRINTS_SAFETY_CAP {
            continue;
        }
        // Population-scaled concurrency cap: ~1 concurrent project per 6 members,
        // floor 2, ceiling MAX_BLUEPRINTS_SAFETY_CAP - 1. Keeps small bands moving
        // without flooding workers; lets larger settlements parallelise civic work.
        let concurrent_cap =
            ((faction.member_count as usize / 6).max(2)).min(MAX_BLUEPRINTS_SAFETY_CAP - 1);
        if count >= concurrent_cap {
            continue;
        }

        let plan = plans.0.get(&faction_id);
        let pending_kinds = pending_kinds_per_faction
            .get(&faction_id)
            .unwrap_or(&empty_pending);
        // Peak population for civic-milestone gates. Falls back to current
        // member_count when the faction has no Settlement entity yet (e.g.
        // first-tick before `auto_found_default_settlements_system` ran).
        let peak_pop = settlement_map
            .first_for_faction(faction_id)
            .and_then(|sid| settlement_map.by_id.get(&sid))
            .and_then(|&e| settlement_q.get(e).ok())
            .map(|s| s.peak_population)
            .unwrap_or(faction.member_count);
        // Brain-readiness gate: when an organic survey has been initiated
        // for this faction's settlement but hasn't completed its first
        // survey (last_survey_tick == 0), suppress shelter-class candidates
        // from the legacy `generate_candidates` fallback. Otherwise the
        // chief would stamp Hut/Longhouse/CompositeHouse into broad legacy
        // zones before parcel-driven placement has had its first pass —
        // leading to houses on planned roads or on land marked for other
        // districts. Non-shelter candidates (Hearth, Granary, civic, etc.)
        // remain allowed so the settlement still gets useful work.
        let brain_for_faction = settlement_map
            .first_for_faction(faction_id)
            .and_then(|sid| maps.organic_brains.0.get(&sid));
        let brain_pending_first_survey = brain_for_faction
            .map(|b| b.last_survey_tick == 0)
            .unwrap_or(false);

        // sleepy-dove Phase 4: buildable surface for this settlement is
        // the poster-pool union of resident chief + architect Learned.
        let settlement_id = settlement_map.first_for_faction(faction_id);
        let available_techs = poster_pool.union_of_learned(faction_id, settlement_id);

        let best = maps
            .organic_selected
            .0
            .get(&faction_id)
            .map(build_candidate_from_organic)
            .or_else(|| {
                let mut candidates = generate_candidates(
                    faction_id,
                    faction,
                    plan,
                    &chunk_map,
                    &maps.as_view(),
                    &bp_map,
                    &*maps.doormat,
                    pending_kinds,
                    &plot_index,
                    &plot_q,
                    peak_pop,
                    None,
                    available_techs,
                );
                if brain_pending_first_survey {
                    candidates.retain(|c| {
                        !matches!(
                            c.intent,
                            BuildIntent::Hut(_)
                                | BuildIntent::Longhouse(_)
                                | BuildIntent::CompositeHouse { .. }
                        )
                    });
                }
                // Stage 3 feedback: penalize candidates whose required inputs include
                // a chronically-deficient good (per `material_deficit_ema`). Skips the
                // upgrade-rebuild path which short-circuits with score 5000 from
                // `generate_candidates`.
                for candidate in candidates.iter_mut() {
                    let required = candidate.intent.required_goods();
                    let mut penalty = 0.0f32;
                    for (rid, qty) in required {
                        if qty == 0 {
                            continue;
                        }
                        let ema = faction.material_deficit_ema_of(rid);
                        if ema >= crate::simulation::projects::DEFICIT_EMA_RARE_THRESHOLD {
                            penalty += 600.0;
                        } else if ema >= 80 {
                            penalty += 200.0;
                        }
                    }
                    candidate.score -= penalty;
                }
                candidates.into_iter().max_by(|a, b| {
                    a.score
                        .partial_cmp(&b.score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
            });
        let Some(best) = best else {
            continue;
        };

        // Land-tenure gate: civic blueprints (chief-posted, no
        // requesting household) only land on `StateOwned` plots of this
        // faction or on wild land outside any plot. Phase 3 is a no-op
        // while every plot is StateOwned of its founding faction; once
        // Phase 4 ships household leases, this prevents the chief from
        // building on now-private plots.
        if !crate::simulation::land::tile_buildable_by(
            &plot_index,
            &plot_q,
            best.tile,
            faction_id,
            None,
        ) {
            continue;
        }
        if candidate_touches_planned_road(&best, faction_id, &settlement_map, &maps.organic_brains)
        {
            continue;
        }

        // Runtime pre-spawn reachability gate. One choke point covers both the
        // organic-selected intent stream and the `generate_candidates`
        // fallback (both flow through `best`): an intent surveyed before a
        // terrain/wall change — or a parcel the planner placed across a river
        // — is refused here rather than spawned as a build no worker can path
        // to. The downstream door-cardinal check only proves doormat→home on
        // *current* terrain; this proves the anchor itself is connected.
        if best.tile != faction.home_tile
            && !crate::simulation::placement_reachability::tile_reachable_from_home(
                &chunk_map,
                faction.home_tile,
                best.tile,
            )
        {
            continue;
        }

        // sleepy-dove Phase 4: resolve the poster for this intent from the
        // pool — chief if they can author every gated part, else the
        // resident architect with the widest coverage. No viable poster
        // → skip the intent (a silent stall is correct here: the band
        // genuinely can't build this yet). Stamps `posted_by` +
        // `design_techs` so tier picks freeze at intent time and
        // `record_tech_use` fires at completion (diffusion).
        //
        // Fallback: factions whose chief entity carries no
        // `PersonKnowledge` (test fixtures, SOLO-ish) have an empty pool;
        // fall back to the legacy chief-knowledge author so existing
        // headless tests keep emitting blueprints.
        let author =
            match poster_pool.select_poster_for_intent(faction_id, settlement_id, best.intent) {
                Some(cap) => Some(cap.author()),
                None => {
                    let chief_fallback = faction.chief_entity.and_then(|chief| {
                        chief_knowledge_q
                            .get(chief)
                            .ok()
                            .map(|k| BlueprintAuthor::new(chief, k.learned_bitset()))
                    });
                    match chief_fallback {
                        // Chief exists but pool had no entry (no settlement
                        // yet at tick 0) — author from chief knowledge if the
                        // chief can actually post this intent; else skip.
                        Some(a) if poster_can_post_intent(best.intent, &a.design_techs) => Some(a),
                        Some(_) => continue,
                        // No chief knowledge at all (pure fixture faction):
                        // emit author-less so legacy behaviour is preserved.
                        None => None,
                    }
                }
            };

        spawn_intent(
            &mut commands,
            &mut bp_map,
            &mut terraform_map,
            &mut pending_footprints,
            &chunk_map,
            &*maps.bed_map,
            &*maps.doormat,
            faction_id,
            faction.home_tile,
            best.intent,
            best.tile,
            best.door_dir,
            author,
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
    maps: &GenCandidatesMaps,
    bp_map: &BlueprintMap,
    doormat: &crate::simulation::doormat::DoormatReservations,
    pending_kinds: &AHashMap<BuildSiteKind, u32>,
    plot_index: &crate::simulation::land::PlotIndex,
    plot_q: &Query<&crate::simulation::land::Plot>,
    peak_pop: u32,
    seed_techs: Option<&FactionTechs>,
    // sleepy-dove Phase 4: runtime buildable surface = union of the
    // settlement's resident chief + architect Learned sets (from
    // `ConstructionPosterPool`). Ignored when `seed_techs` is `Some`
    // (seed mode drives tiers from the era profile instead). Replaces
    // the faction-wide `community_adoption_bitset` gate so a band can
    // build whatever a single member learned.
    available_techs: FactionTechs,
) -> Vec<BuildCandidate> {
    let pending_of = |k: BuildSiteKind| -> u32 { pending_kinds.get(&k).copied().unwrap_or(0) };
    // Walls are tracked per-material (`Wall(Palisade)`, `Wall(Stone)`, …);
    // the wall-deficit gate cares only about total wall blueprints in flight.
    let pending_walls_total: u32 = pending_kinds
        .iter()
        .filter_map(|(k, n)| match k {
            BuildSiteKind::Wall(_) => Some(*n),
            _ => None,
        })
        .sum();
    use crate::simulation::settlement::ZoneKind;

    let mut out: Vec<BuildCandidate> = Vec::with_capacity(8);
    let home = faction.home_tile;
    let members = faction.member_count;
    // sleepy-dove Phase 4: the buildable surface. In seed mode it's the
    // era profile (`seed_techs`); at runtime it's the poster-pool union
    // of resident chief + architect **Learned** sets — NOT the
    // faction-wide `community_adoption_bitset`. A band can build whatever
    // any single resident learned regardless of community adoption stage;
    // adoption is now a downstream emergent signal fed by `record_tech_use`
    // at completion, not a precondition. `select_poster_for_intent` still
    // filters each emitted intent to one poster who can author every part.
    let community_techs = seed_techs.cloned().unwrap_or(available_techs);
    let techs = &community_techs;
    let seed_mode = seed_techs.is_some();
    let culture = &faction.culture;
    // Step 2: route through the era-aware selector. Seed mode passes `None`
    // (unconstrained — materials are stamped for free). Runtime also passes
    // `None` until Step 3 threads the chief-cadence `MaterialAvailabilityView`
    // in, at which point a `Scarce` input keeps this rung + market-hauls it and
    // an all-`Unavailable` ladder returns `EmergencyShelter` (handled at the
    // residential branch in Step 7). `mat()` is always `Some` under `None`.
    let wall_sel = select_wall_material(techs, None);
    let wall_mat = wall_sel.mat().unwrap_or_else(|| best_wall_material(techs));

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
                    door_dir: None,
                });
                return out; // skip everything else this tick
            }
        }
    }

    // 1. Hearths — pacing depends on era:
    //    - Paleolithic/Mesolithic: band camps grow one fire per ~6 members,
    //      gated on *crescent saturation* across all hearths plus a remaining
    //      bed deficit. Bands fill out hearth #1's crescent before #2 opens.
    //    - Neolithic: each hearth represents an extended-family household
    //      cluster (~8 people). A new hearth opens once *every* existing
    //      hearth has ≥ 8 beds inside its 2..6 crescent ring.
    //    - Chalcolithic+: settled cultures keep a single civic-zone hearth
    //      (hearth-per-house remains future work — needs Household component).
    let era = current_era(techs);
    const NEOLITHIC_BEDS_PER_HEARTH: u32 = 8;
    let desired_hearths: u32 = match era {
        Era::Paleolithic | Era::Mesolithic => {
            crate::simulation::settlement::paleolithic_hearth_count(members)
        }
        Era::Neolithic => {
            ((members + NEOLITHIC_BEDS_PER_HEARTH - 1) / NEOLITHIC_BEDS_PER_HEARTH).max(1)
        }
        _ => 1,
    };
    // Counts include both built structures *and* in-flight blueprints, so a
    // structure already queued in `GatherMaterials` is treated as fulfilling
    // its own gate — without this, every chief tick re-queues another of the
    // same kind until the first one finishes building.
    let built_hearths = count_campfires_near(&maps.campfire_map, home, 30) as u32;
    let existing_hearths = built_hearths.saturating_add(pending_of(BuildSiteKind::Campfire));
    let built_beds = count_beds_near(&maps.bed_map, home, 30) as i32;
    let bed_count = built_beds + pending_of(BuildSiteKind::Bed) as i32;
    let bed_deficit_pre = (members as i32 - bed_count).max(0);
    let gate_ok = if existing_hearths == 0 {
        true
    } else if matches!(era, Era::Paleolithic | Era::Mesolithic) {
        let hearths: Vec<(i32, i32)> = maps
            .campfire_map
            .0
            .keys()
            .copied()
            .filter(|&(cx, cy)| {
                (cx as i32 - home.0 as i32).abs() <= 25 && (cy as i32 - home.1 as i32).abs() <= 25
            })
            .collect();
        let crescents_saturated = !hearths.is_empty()
            && find_bed_tile_around_hearth(
                chunk_map,
                &maps.bed_map,
                bp_map,
                doormat,
                &hearths,
                home,
                2,
                6,
            )
            .is_none();
        crescents_saturated && bed_deficit_pre > 0
    } else if matches!(era, Era::Neolithic) {
        // One hearth per ~8-person extended-family cluster. `desired_hearths`
        // already caps the total at ceil(members/8); require each existing
        // hearth to be "earning" its 8-person share before opening another.
        // The old gate inspected a paleo crescent ring around each hearth —
        // meaningless at Neolithic, where beds live *inside* walled huts the
        // seed planner clusters near home, incidentally overlapping the ring
        // and tripping the gate into queueing redundant campfires.
        existing_hearths < desired_hearths
            && (existing_hearths as u32) * NEOLITHIC_BEDS_PER_HEARTH <= members
    } else {
        false // Chalcolithic+: single civic hearth
    };
    let effective_desired = if gate_ok {
        desired_hearths
    } else {
        existing_hearths
    };
    if techs.has(FIRE_MAKING) && existing_hearths < effective_desired {
        let mut tile_opt = find_unfilled_civic_zone_tile(
            chunk_map,
            &maps.bed_map,
            bp_map,
            doormat,
            &maps.campfire_map,
            plan,
            home,
        );
        // Bootstrap: a freshly-created Paleolithic faction may not have its
        // first plan yet (planner is staggered ~60 ticks). Use the
        // deterministic hearth offsets directly so even the very first fire
        // lands at the proper distance from home rather than adjacent to it.
        if tile_opt.is_none() && !techs.has(PERM_SETTLEMENT) && plan.is_none() {
            let hearths = crate::simulation::settlement::paleolithic_hearth_positions_river_aware(
                chunk_map, faction_id, home, members,
            );
            'outer: for (hx, hy) in hearths {
                for dy in -1i32..=1 {
                    for dx in -1i32..=1 {
                        let tx = hx + dx;
                        let ty = hy + dy;
                        let pos = (tx as i32, ty as i32);
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
                door_dir: None,
            });
        }
    }

    let bed_deficit = bed_deficit_pre as f32;

    // Step 7: era-appropriate emergency shelter. At runtime (never seed —
    // seed stamps materials for free), if every wall ladder rung is
    // unobtainable (`select_wall_material → EmergencyShelter`: not stored,
    // not raw-gatherable, not affordably procurable) a Neolithic+ band would
    // otherwise stall shelter-less forever. Emit a low-score bare `Bed`
    // candidate on a deterministic era-keyed annulus so the band gets
    // *something* — the walled-house candidate below is still emitted and
    // far out-scores this, so the moment any real wall material arrives the
    // band resumes proper huts and these emergency beds stop. Paleo/Meso are
    // excluded (their crescent-bed branch is the native pattern). Non-shelter
    // builds get no previous-era substitute — they simply defer (no
    // candidate), which is the pre-existing behaviour.
    let emergency_shelter = !seed_mode
        && bed_deficit > 0.0
        && !matches!(era, Era::Paleolithic | Era::Mesolithic)
        && !faction.material_view.is_empty()
        && matches!(
            select_wall_material(techs, Some(&faction.material_view)),
            WallSelection::EmergencyShelter
        );
    if emergency_shelter {
        let layout_seed = plan.map(|p| p.culture_hash).unwrap_or(faction_id as u64);
        if let Some(tile) = find_emergency_bed_tile(
            chunk_map,
            &maps.bed_map,
            bp_map,
            doormat,
            home,
            era,
            layout_seed,
            bed_count,
        ) {
            out.push(BuildCandidate {
                intent: BuildIntent::Single(BuildSiteKind::Bed),
                tile,
                // Below every walled-house score (Hut 230+, Longhouse 260+)
                // so proper shelter always wins once buildable.
                score: 100.0 + bed_deficit * 20.0,
                door_dir: None,
            });
        }
    }

    // 2. Residential — the principal growth axis. Pre-settlement: simple beds.
    //    Post-settlement: walled huts; with CITY_STATE_ORG, longhouses preferred.
    if bed_deficit > 0.0 {
        if matches!(era, Era::Paleolithic | Era::Mesolithic) {
            // Paleolithic/Mesolithic: cluster beds in a crescent annulus
            // around the nearest hearth. Defer if no fire exists yet — the
            // campfire candidate above (score 1000) will resolve first,
            // matching the historical ordering of fire-then-shelter.
            // Era-gated (not `!techs.has(PERM_SETTLEMENT)`) so a Neolithic+
            // band never degenerates to outdoor beds if the poster pool
            // transiently lacks PERM_SETTLEMENT (e.g. chief-death gap) — it
            // simply emits no bed candidate that tick and retries.
            let hearths: Vec<(i32, i32)> = maps
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
                    doormat,
                    &hearths,
                    home,
                    2,
                    6,
                ) {
                    out.push(BuildCandidate {
                        intent: BuildIntent::Single(BuildSiteKind::Bed),
                        tile,
                        score: 200.0 + bed_deficit * 30.0,
                        door_dir: None,
                    });
                }
            }
        } else if !seed_mode && faction.material_view.is_empty() {
            // Step 7: scarcity not yet classified this chief window. Emitting
            // a higher-tier hut now would stall (no material) and fill the
            // per-faction concurrency cap, after which the emergency Bed can
            // never be *selected*. Defer one window — the emergency block
            // above handles the truly-unobtainable case once classified.
        } else if emergency_shelter {
            // Emergency Bed already emitted above. Suppress the walled house
            // so it can't out-score the fallback or hog the concurrency cap
            // with a blueprint that can never finalize.
        } else if techs.has(CITY_STATE_ORG) && bed_deficit >= 2.0 {
            // Frontage-first: prefer vacant residential lots whose access tile
            // sits on the carved spine; fall back to zone-area scoring.
            // Layout-seed roll: Longhouse is the default when bed deficit is
            // large (≥4), but for smaller deficits we roll for visual variety.
            // Seed mixes plan.culture_hash with current bed_count so successive
            // builds in the same faction don't all pick the same footprint.
            let layout_seed =
                plan.map(|p| p.culture_hash).unwrap_or(faction_id as u64) ^ (bed_count as u64);
            let mut roll_rng = fastrand::Rng::with_seed(layout_seed);
            let try_longhouse = if bed_deficit >= 4.0 {
                true
            } else {
                roll_rng.f32() < 0.6
            };
            // Composite L-shapes are temporarily disabled for shelter
            // auto-builds: the current 2×2 + 2×1 mask has no true interior
            // cells, so `plan_composite_building` emits walls/door but no
            // beds. Keep layout stable with huts/longhouses until the
            // composite template grows a real room interior.
            let try_lshape = false;
            if try_lshape {
                // L-shape main block (2×2) + east extension (2×1) — 4×2
                // bounding box. Use the bounding-box search to find a
                // candidate anchor, then verify the actual mask via
                // `is_clear_shape` so non-rectangular interior cells aren't
                // dropped onto impassable terrain.
                let lshape = crate::simulation::building_template::FootprintShape::LShape {
                    w1: 2,
                    h1: 2,
                    w2: 2,
                    h2: 1,
                };
                if let Some(origin) = find_footprint_in_zone(
                    chunk_map,
                    &maps.bed_map,
                    bp_map,
                    doormat,
                    plan,
                    ZoneKind::Residential,
                    home,
                    2,
                    1,
                    18,
                ) {
                    let rotation = crate::simulation::building_template::Rotation::R0;
                    if is_clear_shape(
                        chunk_map,
                        &maps.bed_map,
                        bp_map,
                        doormat,
                        lshape,
                        rotation,
                        origin,
                    ) {
                        out.push(BuildCandidate {
                            intent: BuildIntent::CompositeHouse {
                                shape: lshape,
                                rotation,
                                wall_material: wall_mat,
                            },
                            tile: origin,
                            score: 245.0 + bed_deficit * 25.0,
                            door_dir: None,
                        });
                    }
                }
            }
            let lh_origin: Option<((i32, i32), Option<crate::simulation::land::TileEdge>)> =
                if try_longhouse {
                    find_footprint_at_frontage_lot(
                        chunk_map,
                        &maps.bed_map,
                        bp_map,
                        doormat,
                        plot_index,
                        plot_q,
                        faction_id,
                        ZoneKind::Residential,
                        home,
                        2,
                        1,
                    )
                    .map(|(t, e)| (t, Some(e)))
                    .or_else(|| {
                        find_footprint_in_zone(
                            chunk_map,
                            &maps.bed_map,
                            bp_map,
                            doormat,
                            plan,
                            ZoneKind::Residential,
                            home,
                            2,
                            1,
                            20,
                        )
                        .map(|t| (t, None))
                    })
                } else {
                    None
                };
            if let Some((origin, edge)) = lh_origin {
                out.push(BuildCandidate {
                    intent: BuildIntent::Longhouse(wall_mat),
                    tile: origin,
                    score: 260.0 + bed_deficit * 25.0,
                    door_dir: edge,
                });
            } else {
                let hut_origin: Option<((i32, i32), Option<crate::simulation::land::TileEdge>)> =
                    find_footprint_at_frontage_lot(
                        chunk_map,
                        &maps.bed_map,
                        bp_map,
                        doormat,
                        plot_index,
                        plot_q,
                        faction_id,
                        ZoneKind::Residential,
                        home,
                        1,
                        1,
                    )
                    .map(|(t, e)| (t, Some(e)))
                    .or_else(|| {
                        find_footprint_in_zone(
                            chunk_map,
                            &maps.bed_map,
                            bp_map,
                            doormat,
                            plan,
                            ZoneKind::Residential,
                            home,
                            1,
                            1,
                            18,
                        )
                        .map(|t| (t, None))
                    });
                if let Some((origin, edge)) = hut_origin {
                    out.push(BuildCandidate {
                        intent: BuildIntent::Hut(wall_mat),
                        tile: origin,
                        score: 230.0 + bed_deficit * 25.0,
                        door_dir: edge,
                    });
                }
            }
        } else {
            let hut_origin: Option<((i32, i32), Option<crate::simulation::land::TileEdge>)> =
                find_footprint_at_frontage_lot(
                    chunk_map,
                    &maps.bed_map,
                    bp_map,
                    doormat,
                    plot_index,
                    plot_q,
                    faction_id,
                    ZoneKind::Residential,
                    home,
                    1,
                    1,
                )
                .map(|(t, e)| (t, Some(e)))
                .or_else(|| {
                    find_footprint_in_zone(
                        chunk_map,
                        &maps.bed_map,
                        bp_map,
                        doormat,
                        plan,
                        ZoneKind::Residential,
                        home,
                        1,
                        1,
                        18,
                    )
                    .map(|t| (t, None))
                });
            if let Some((origin, edge)) = hut_origin {
                out.push(BuildCandidate {
                    intent: BuildIntent::Hut(wall_mat),
                    tile: origin,
                    score: 230.0 + bed_deficit * 25.0,
                    door_dir: edge,
                });
            }
        }
    }

    // 3. Defense — palisade reinforcement. Driven by culture.defensive.
    //    Pre-Chalcolithic societies don't fortify: Paleolithic bands and
    //    Neolithic farming villages were typically unwalled. Defensive
    //    perimeters become standard with Chalcolithic/Bronze Age city-states.
    if matches!(era, Era::Chalcolithic | Era::BronzeAge) {
        let walls_count =
            count_walls_near(&maps.wall_map, home, 25) as i32 + pending_walls_total as i32;
        let target_walls = (members as i32 * 2 + 8).min(48);
        let defense_deficit = (target_walls - walls_count).max(0) as f32;
        if defense_deficit > 0.0 && bed_count > 0 {
            if let Some(tile) =
                find_palisade_site(chunk_map, &maps.bed_map, bp_map, doormat, home, 2)
            {
                let def_mult = 0.4 + (culture.defensive as f32 / 255.0) * 1.6;
                out.push(BuildCandidate {
                    intent: BuildIntent::PalisadeSegment(wall_mat, 2),
                    tile,
                    score: 70.0 * def_mult + defense_deficit * 1.5,
                    door_dir: None,
                });
            }
        }
    }

    // 4. Crafting — Workbench when FLINT_KNAPPING and we have somewhere to live.
    if techs.has(FLINT_KNAPPING)
        && count_workbenches_near(&maps.workbench_map, home, 25)
            + pending_of(BuildSiteKind::Workbench) as usize
            == 0
        && bed_count >= 2
    {
        if let Some(tile) = find_clear_tile_in_zone(
            chunk_map,
            &maps.bed_map,
            bp_map,
            doormat,
            plan,
            ZoneKind::Crafting,
            home,
            10,
        ) {
            out.push(BuildCandidate {
                intent: BuildIntent::Single(BuildSiteKind::Workbench),
                tile,
                score: 150.0,
                door_dir: None,
            });
        }
    }

    // 5. Granary — gated by GRANARY tech + (era, peak_pop) milestone.
    if techs.has(GRANARY)
        && count_granaries_near(&maps.granary_map, home, 25)
            + pending_of(BuildSiteKind::Granary) as usize
            == 0
        && (seed_mode
            || crate::simulation::civic_milestones::civic_milestone_allows(
                crate::simulation::civic_milestones::CivicKind::Granary,
                era,
                peak_pop,
            ))
    {
        if let Some(tile) = find_clear_tile_in_zone(
            chunk_map,
            &maps.bed_map,
            bp_map,
            doormat,
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
                door_dir: None,
            });
        }
    }

    // 5b. Well — gated by WELL_DIGGING. Per-era target: Paleo/Meso 0,
    // Neo 1, Chalco 2, Bronze 3. Seed mode bypasses civic gates.
    if techs.has(WELL_DIGGING) {
        let target_wells = match era {
            Era::Paleolithic | Era::Mesolithic => 0,
            Era::Neolithic => 1,
            Era::Chalcolithic => 2,
            Era::BronzeAge => 3,
        };
        if target_wells > 0 {
            let built = count_wells_near(&maps.well_map, home, 25);
            let pending = pending_of(BuildSiteKind::Well) as usize;
            if built + pending < target_wells {
                // Try civic, then storage, then residential zones.
                let zone = [ZoneKind::Civic, ZoneKind::Storage, ZoneKind::Residential]
                    .into_iter()
                    .find_map(|z| {
                        find_clear_tile_in_zone(
                            chunk_map,
                            &maps.bed_map,
                            bp_map,
                            doormat,
                            plan,
                            z,
                            home,
                            12,
                        )
                    });
                if let Some(tile) = zone {
                    out.push(BuildCandidate {
                        intent: BuildIntent::Single(BuildSiteKind::Well),
                        tile,
                        score: 175.0,
                        door_dir: None,
                    });
                }
            }
        }
    }

    // 6. Shrine — gated by SACRED_RITUAL + (era, peak_pop) milestone.
    if techs.has(SACRED_RITUAL)
        && count_shrines_near(&maps.shrine_map, home, 25)
            + pending_of(BuildSiteKind::Shrine) as usize
            == 0
        && (seed_mode
            || crate::simulation::civic_milestones::civic_milestone_allows(
                crate::simulation::civic_milestones::CivicKind::Shrine,
                era,
                peak_pop,
            ))
    {
        if let Some(tile) = find_clear_tile_in_zone(
            chunk_map,
            &maps.bed_map,
            bp_map,
            doormat,
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
                door_dir: None,
            });
        }
    }

    // 7. Market — LONG_DIST_TRADE + (era, peak_pop) milestone.
    if techs.has(LONG_DIST_TRADE)
        && (seed_mode
            || crate::simulation::civic_milestones::civic_milestone_allows(
                crate::simulation::civic_milestones::CivicKind::Market,
                era,
                peak_pop,
            ))
    {
        let target_count = if culture.mercantile > 180 { 2 } else { 1 };
        let market_count = count_markets_near(&maps.market_map, home, 25)
            + pending_of(BuildSiteKind::Market) as usize;
        if market_count < target_count {
            if let Some(tile) = find_clear_tile_in_zone(
                chunk_map,
                &maps.bed_map,
                bp_map,
                doormat,
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
                    door_dir: None,
                });
            }
        }
    }

    // 8. Barracks — PROFESSIONAL_ARMY + (era, peak_pop) milestone.
    if techs.has(PROFESSIONAL_ARMY)
        && count_barracks_near(&maps.barracks_map, home, 25)
            + pending_of(BuildSiteKind::Barracks) as usize
            == 0
        && (seed_mode
            || crate::simulation::civic_milestones::civic_milestone_allows(
                crate::simulation::civic_milestones::CivicKind::Barracks,
                era,
                peak_pop,
            ))
    {
        if let Some(tile) = find_clear_tile_in_zone(
            chunk_map,
            &maps.bed_map,
            bp_map,
            doormat,
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
                door_dir: None,
            });
        }
    }

    // 9. Monument — MONUMENTAL_BUILDING + (era, peak_pop) milestone.
    if techs.has(MONUMENTAL_BUILDING)
        && count_monuments_near(&maps.monument_map, home, 30)
            + pending_of(BuildSiteKind::Monument) as usize
            == 0
        && (seed_mode
            || crate::simulation::civic_milestones::civic_milestone_allows(
                crate::simulation::civic_milestones::CivicKind::Monument,
                era,
                peak_pop,
            ))
    {
        if let Some(tile) = find_clear_tile_in_zone(
            chunk_map,
            &maps.bed_map,
            bp_map,
            doormat,
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
                door_dir: None,
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
    bed_map: &BedMap,
    doormat: &crate::simulation::doormat::DoormatReservations,
    faction_id: u32,
    home: (i32, i32),
    intent: BuildIntent,
    tile: (i32, i32),
    door_dir: Option<crate::simulation::land::TileEdge>,
    author: Option<BlueprintAuthor>,
) {
    match intent {
        BuildIntent::Single(kind) => {
            let target_z = chunk_map.surface_z_at(tile.0 as i32, tile.1 as i32) as i8;
            let wp = tile_to_world(tile.0 as i32, tile.1 as i32);
            let e = commands
                .spawn((
                    Blueprint::new(faction_id, None, kind, tile, target_z).with_author(author),
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
                bed_map,
                doormat,
                tile.0 as i32,
                tile.1 as i32,
                1,
                1,
                faction_id,
                home,
                &[(0, 0)],
                wall_mat,
                door_dir,
                author,
            );
        }
        BuildIntent::Longhouse(wall_mat) => {
            plan_building(
                commands,
                bp_map,
                terraform_map,
                pending_footprints,
                chunk_map,
                bed_map,
                doormat,
                tile.0 as i32,
                tile.1 as i32,
                2,
                1,
                faction_id,
                home,
                &[(-1, 0), (1, 0)],
                wall_mat,
                door_dir,
                author,
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
                    )
                    .with_author(author),
                    Transform::from_xyz(wp.x, wp.y, 0.3),
                    GlobalTransform::default(),
                    Visibility::Visible,
                    InheritedVisibility::default(),
                ))
                .id();
            bp_map.0.insert(tile, e);
        }
        BuildIntent::CompositeHouse {
            shape,
            rotation,
            wall_material,
        } => {
            plan_composite_building(
                commands,
                bp_map,
                chunk_map,
                bed_map,
                doormat,
                tile,
                shape,
                rotation,
                wall_material,
                faction_id,
                home,
                door_dir,
                author,
            );
        }
    }
}

/// Seed-mode mirror of `spawn_intent`: stamps tiles + spawns structure
/// entities directly instead of emitting `Blueprint`s for workers to
/// construct. Drives the unified `generate_candidates` candidate stream at
/// `OnEnter(Playing)` so the seed pipeline goes through the same intent
/// logic the runtime chief uses (rather than duplicating placement logic
/// in seed-only helpers). Returns `true` if the intent was applied; the
/// caller tracks `used` tiles to avoid restamping.
///
/// Composite houses fall back to `plan_composite_building` (blueprint emit)
/// at seed time — the runtime chief picks them up if conditions still hold.
/// In practice they're rolled only ~10% on small bed deficits, so the seed
/// loop usually picks Hut / Longhouse anyway.
fn seed_single_tile_clear(
    tile: (i32, i32),
    used: &AHashSet<(i32, i32)>,
    maps: &FurnitureMaps,
    chunk_map: &ChunkMap,
    doormat: &crate::simulation::doormat::DoormatReservations,
) -> bool {
    if used.contains(&tile) || doormat.is_reserved(tile) {
        return false;
    }
    if !chunk_map.is_passable(tile.0, tile.1) {
        return false;
    }
    let Some(k) = chunk_map.tile_kind_at(tile.0, tile.1) else {
        return false;
    };
    if k == TileKind::Wall || k == TileKind::Stone || k == TileKind::Road {
        return false;
    }
    !maps.bed_map.0.contains_key(&tile)
        && !maps.wall_map.0.contains_key(&tile)
        && !maps.campfire_map.0.contains_key(&tile)
        && !maps.door_map.0.contains_key(&tile)
        && !maps.workbench_map.0.contains_key(&tile)
        && !maps.loom_map.0.contains_key(&tile)
        && !maps.table_map.0.contains_key(&tile)
        && !maps.chair_map.0.contains_key(&tile)
        && !maps.granary_map.0.contains_key(&tile)
        && !maps.shrine_map.0.contains_key(&tile)
        && !maps.market_map.0.contains_key(&tile)
        && !maps.barracks_map.0.contains_key(&tile)
        && !maps.monument_map.0.contains_key(&tile)
        && !maps.bridge_map.0.contains_key(&tile)
        && !maps.well_map.0.contains_key(&tile)
}

fn find_clear_seed_single_tile(
    anchor: (i32, i32),
    used: &AHashSet<(i32, i32)>,
    maps: &FurnitureMaps,
    chunk_map: &ChunkMap,
    doormat: &crate::simulation::doormat::DoormatReservations,
    max_radius: i32,
) -> Option<(i32, i32)> {
    for ring in 0..=max_radius {
        for dy in -ring..=ring {
            for dx in -ring..=ring {
                if dx.abs().max(dy.abs()) != ring {
                    continue;
                }
                let tile = (anchor.0 + dx, anchor.1 + dy);
                if seed_single_tile_clear(tile, used, maps, chunk_map, doormat) {
                    return Some(tile);
                }
            }
        }
    }
    None
}

fn seed_house_footprint_clear(
    anchor: (i32, i32),
    used: &AHashSet<(i32, i32)>,
    maps: &FurnitureMaps,
    chunk_map: &ChunkMap,
    doormat: &crate::simulation::doormat::DoormatReservations,
    half_w: i32,
    half_h: i32,
) -> bool {
    for dy in -half_h..=half_h {
        for dx in -half_w..=half_w {
            let tile = (anchor.0 + dx, anchor.1 + dy);
            if !seed_single_tile_clear(tile, used, maps, chunk_map, doormat) {
                return false;
            }
        }
    }
    true
}

fn seed_walled_house_or_nearby(
    commands: &mut Commands,
    maps: &mut FurnitureMaps,
    chunk_map: &mut ChunkMap,
    tile_changed: &mut EventWriter<crate::world::chunk_streaming::TileChangedEvent>,
    used: &mut AHashSet<(i32, i32)>,
    anchor: (i32, i32),
    half_w: i32,
    half_h: i32,
    interior_beds: &[(i32, i32)],
    wall_material: WallMaterial,
    faction_id: u32,
    home: (i32, i32),
    door_dir: Option<crate::simulation::land::TileEdge>,
    doormat: &mut crate::simulation::doormat::DoormatReservations,
    road_carve: &mut RoadCarveQueue,
    seed_techs: &FactionTechs,
) -> Option<(i32, i32)> {
    if seed_walled_house_at(
        commands,
        maps,
        chunk_map,
        tile_changed,
        used,
        anchor.0,
        anchor.1,
        half_w,
        half_h,
        interior_beds,
        wall_material,
        faction_id,
        home,
        door_dir,
        doormat,
        road_carve,
        seed_techs,
    ) {
        return Some(anchor);
    }

    // `generate_candidates` is shared with runtime planning and cannot see the
    // seed pass's transient `used` reservations. On real terrain, this can
    // produce a good-looking residential anchor that fails to stamp because a
    // freshly-seeded yard, doormat, or failed candidate already claimed part of
    // it. Keep the startup capacity goal moving by searching outward from that
    // planned anchor and using the normal verified house stamper at the first
    // nearby footprint that actually works.
    const SEED_HOUSE_RELOCATE_RADIUS: i32 = 24;
    for ring in 1..=SEED_HOUSE_RELOCATE_RADIUS {
        for dy in -ring..=ring {
            for dx in -ring..=ring {
                if dx.abs().max(dy.abs()) != ring {
                    continue;
                }
                let candidate = (anchor.0 + dx, anchor.1 + dy);
                if blocks_cardinal_corridor(candidate.0, candidate.1, half_w, half_h, home) {
                    continue;
                }
                if !seed_house_footprint_clear(
                    candidate, used, maps, chunk_map, doormat, half_w, half_h,
                ) {
                    continue;
                }
                let (_, spread) =
                    footprint_z_stats(chunk_map, candidate.0, candidate.1, half_w, half_h);
                if spread > MAX_TERRAFORM_SPREAD {
                    continue;
                }
                if seed_walled_house_at(
                    commands,
                    maps,
                    chunk_map,
                    tile_changed,
                    used,
                    candidate.0,
                    candidate.1,
                    half_w,
                    half_h,
                    interior_beds,
                    wall_material,
                    faction_id,
                    home,
                    None,
                    doormat,
                    road_carve,
                    seed_techs,
                ) {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

fn seed_apply_intent(
    commands: &mut Commands,
    maps: &mut FurnitureMaps,
    chunk_map: &mut ChunkMap,
    tile_changed: &mut EventWriter<crate::world::chunk_streaming::TileChangedEvent>,
    used: &mut AHashSet<(i32, i32)>,
    doormat: &mut crate::simulation::doormat::DoormatReservations,
    road_carve: &mut RoadCarveQueue,
    faction_id: u32,
    home: (i32, i32),
    intent: BuildIntent,
    tile: (i32, i32),
    door_dir: Option<crate::simulation::land::TileEdge>,
    seed_techs: &FactionTechs,
) -> Option<(i32, i32)> {
    match intent {
        BuildIntent::Single(kind) => {
            // Seed candidate generation is intentionally shared with the
            // runtime chief and cannot see the per-faction `used` set. If the
            // selected civic anchor is the reserved home tile or another
            // freshly-stamped seed tile, nudge the single-tile structure to the
            // nearest valid neighbor instead of starving the rest of the seed
            // loop on the same high-score candidate.
            let Some(place_tile) =
                find_clear_seed_single_tile(tile, used, maps, chunk_map, doormat, 8)
            else {
                return None;
            };
            spawn_seeded_structure_at_tile(
                commands,
                maps,
                chunk_map,
                tile_changed,
                used,
                place_tile,
                faction_id,
                kind,
                seed_techs,
            );
            Some(place_tile)
        }
        BuildIntent::Hut(wall_mat) => seed_walled_house_or_nearby(
            commands,
            maps,
            chunk_map,
            tile_changed,
            used,
            (tile.0, tile.1),
            1,
            1,
            &[(0, 0)],
            wall_mat,
            faction_id,
            home,
            door_dir,
            doormat,
            road_carve,
            seed_techs,
        ),
        BuildIntent::Longhouse(wall_mat) => seed_walled_house_or_nearby(
            commands,
            maps,
            chunk_map,
            tile_changed,
            used,
            (tile.0, tile.1),
            2,
            1,
            &[(-1, 0), (1, 0)],
            wall_mat,
            faction_id,
            home,
            door_dir,
            doormat,
            road_carve,
            seed_techs,
        ),
        BuildIntent::PalisadeSegment(wall_mat, _) => seed_apply_wall_tile(
            commands,
            maps,
            chunk_map,
            tile_changed,
            used,
            doormat,
            tile,
            wall_mat,
        )
        .then_some(tile),
        BuildIntent::CompositeHouse { .. } => {
            // Seed defers composite footprints to the runtime chief — the
            // shape-aware door picker in `plan_composite_building` needs
            // blueprint-aware tile checks, and the candidate is rare
            // enough at seed time (10% roll on modest bed deficit) that
            // skipping it loses ~zero variety.
            None
        }
    }
}

/// Stamp a single Wall tile at `tile` and spawn the matching `Wall` entity.
/// Used by the seed-mode resolver for `BuildIntent::PalisadeSegment` so the
/// candidate generator's per-segment palisade emission progresses tile-by-tile
/// at game start. Refuses tiles that are already used / impassable / already a
/// Wall or Road / reserved as someone's doormat.
fn seed_apply_wall_tile(
    commands: &mut Commands,
    maps: &mut FurnitureMaps,
    chunk_map: &mut ChunkMap,
    tile_changed: &mut EventWriter<crate::world::chunk_streaming::TileChangedEvent>,
    used: &mut AHashSet<(i32, i32)>,
    doormat: &crate::simulation::doormat::DoormatReservations,
    tile: (i32, i32),
    material: WallMaterial,
) -> bool {
    if used.contains(&tile) || doormat.is_reserved(tile) {
        return false;
    }
    if !chunk_map.is_passable(tile.0, tile.1) {
        return false;
    }
    let Some(k) = chunk_map.tile_kind_at(tile.0, tile.1) else {
        return false;
    };
    if k == TileKind::Wall || k == TileKind::Road {
        return false;
    }
    let surf_z = chunk_map.surface_z_at(tile.0, tile.1);
    chunk_map.set_tile(
        tile.0,
        tile.1,
        surf_z + 1,
        TileData {
            kind: TileKind::Wall,
            elevation: 0,
            fertility: 0,
            flags: 0b0001,
            ore: 0,
        },
    );
    let world_pos = tile_to_world(tile.0, tile.1);
    let e = commands
        .spawn((
            Wall { material },
            StructureLabel(material.label()),
            Transform::from_xyz(world_pos.x, world_pos.y, 0.4),
            GlobalTransform::default(),
            Visibility::Visible,
            InheritedVisibility::default(),
        ))
        .id();
    maps.wall_map.0.insert(tile, e);
    used.insert(tile);
    tile_changed.send(crate::world::chunk_streaming::TileChangedEvent {
        tx: tile.0,
        ty: tile.1,
    });
    true
}

/// Place wall / door / bed blueprints over an arbitrary shape mask. Perimeter
/// tiles (any cardinal neighbour outside the mask) become Wall; the one
/// perimeter tile closest to the door cardinal becomes Door; interior tiles
/// become Bed. No terraforming is performed (composite houses anchor to the
/// most-level cell during selection); uneven ground is left to a future
/// shape-aware terraform extension.
fn plan_composite_building(
    commands: &mut Commands,
    bp_map: &mut BlueprintMap,
    chunk_map: &ChunkMap,
    bed_map: &BedMap,
    doormat_res: &crate::simulation::doormat::DoormatReservations,
    anchor: (i32, i32),
    shape: crate::simulation::building_template::FootprintShape,
    rotation: crate::simulation::building_template::Rotation,
    wall_material: WallMaterial,
    faction_id: u32,
    camp_home: (i32, i32),
    door_dir: Option<crate::simulation::land::TileEdge>,
    author: Option<BlueprintAuthor>,
) {
    use crate::simulation::building_template::shape_tiles;
    let tiles = shape_tiles(shape, anchor, rotation);
    if tiles.is_empty() {
        return;
    }
    let tile_set: ahash::AHashSet<(i32, i32)> = tiles.iter().copied().collect();

    // Door direction: prefer the sourced cardinal (plot frontage); else fall
    // back to the cardinal-toward-home rule used by `plan_building`.
    let preferred_edge =
        door_dir.unwrap_or_else(|| crate::simulation::land::TileEdge::toward(anchor, camp_home));

    // Among the four cardinals, find one whose chosen perimeter cell's
    // cardinal-out tile is genuinely outside the mask AND a clear doormat
    // target. Without this gate, an L-shape can place a door on a perimeter
    // cell whose "outside" cardinal hits another wall blueprint, leaving the
    // door unreachable.
    use crate::simulation::land::TileEdge;
    let cardinals = [
        TileEdge::North,
        TileEdge::East,
        TileEdge::South,
        TileEdge::West,
    ];
    let pick_perim_for_edge = |edge: TileEdge| -> Option<((i32, i32), (i32, i32), i64)> {
        let (ddx, ddy) = edge.delta();
        let mut best: Option<((i32, i32), (i32, i32), i64)> = None;
        for &(tx, ty) in &tiles {
            let outside = (tx + ddx, ty + ddy);
            if tile_set.contains(&outside) {
                continue;
            }
            let is_perim = [(1, 0), (-1, 0), (0, 1), (0, -1)]
                .iter()
                .any(|&(ox, oy)| !tile_set.contains(&(tx + ox, ty + oy)));
            if !is_perim {
                continue;
            }
            if !doormat_tile_clear(chunk_map, bed_map, bp_map, doormat_res, outside) {
                continue;
            }
            let d = ((outside.0 - camp_home.0) as i64).pow(2)
                + ((outside.1 - camp_home.1) as i64).pow(2);
            if best.map(|(_, _, bd)| d < bd).unwrap_or(true) {
                best = Some(((tx, ty), outside, d));
            }
        }
        best
    };
    let mut chosen: Option<((i32, i32), TileEdge)> =
        pick_perim_for_edge(preferred_edge).map(|(door, _outside, _)| (door, preferred_edge));
    if chosen.is_none() {
        chosen = cardinals
            .iter()
            .copied()
            .filter(|&e| e != preferred_edge)
            .filter_map(|e| pick_perim_for_edge(e).map(|(door, _, d)| (door, e, d)))
            .min_by_key(|&(_, _, d)| d)
            .map(|(door, e, _)| (door, e));
    }
    let Some((picked_door_tile, door_edge)) = chosen else {
        return; // every cardinal blocked — abort the build
    };
    let door_tile: Option<(i32, i32)> = Some(picked_door_tile);

    // Simulated-build reachability gate: classify the footprint exactly as the
    // spawn loop below does (perim → Wall, interior → Bed, the picked cell →
    // Door), then verify doormat→home and door→every bed with the finished
    // walls in place. Composite L/U shapes can otherwise seal an interior bed.
    {
        let (ddx, ddy) = door_edge.delta();
        let doormat = (picked_door_tile.0 + ddx, picked_door_tile.1 + ddy);
        let mut walls: ahash::AHashSet<(i32, i32)> = ahash::AHashSet::new();
        let mut beds: Vec<(i32, i32)> = Vec::new();
        for &(tx, ty) in &tiles {
            let pos = (tx, ty);
            if Some(pos) == door_tile {
                continue;
            }
            let is_perim = [(1, 0), (-1, 0), (0, 1), (0, -1)]
                .iter()
                .any(|&(ox, oy)| !tile_set.contains(&(tx + ox, ty + oy)));
            if is_perim {
                walls.insert(pos);
            } else {
                beds.push(pos);
            }
        }
        if !crate::simulation::placement_reachability::simulate_house_reachable(
            chunk_map,
            camp_home,
            doormat,
            picked_door_tile,
            &walls,
            &beds,
        ) {
            return;
        }
    }

    // Same target_z as the rectangular path: use the anchor's surface_z so
    // composite walls form a consistent floor. (Spread checks happen in the
    // caller via `shape_z_stats`.)
    let target_z = chunk_map.surface_z_at(anchor.0, anchor.1) as i8;

    for &(tx, ty) in &tiles {
        let pos = (tx, ty);
        if bp_map.0.contains_key(&pos) {
            continue;
        }
        let is_perim = [(1, 0), (-1, 0), (0, 1), (0, -1)]
            .iter()
            .any(|&(ox, oy)| !tile_set.contains(&(tx + ox, ty + oy)));
        let kind = if Some(pos) == door_tile {
            BuildSiteKind::Door
        } else if is_perim {
            BuildSiteKind::Wall(wall_material)
        } else {
            BuildSiteKind::Bed
        };
        let edge = if Some(pos) == door_tile {
            Some(door_edge)
        } else {
            None
        };
        let wp = tile_to_world(tx, ty);
        let mut bp = Blueprint::new(faction_id, None, kind, pos, target_z).with_author(author);
        if let Some(e) = edge {
            bp = bp.with_door_dir(e);
        }
        let e = commands
            .spawn((
                bp,
                Transform::from_xyz(wp.x, wp.y, 0.3),
                GlobalTransform::default(),
                Visibility::Visible,
                InheritedVisibility::default(),
            ))
            .id();
        bp_map.0.insert(pos, e);
    }
}

// ── Ritual system ─────────────────────────────────────────────────────────────

/// Single ritual event record. Most recent N entries are kept on
/// `RitualState.recent_events` and surfaced in the Debug panel.
#[derive(Clone, Debug)]
pub struct RitualEvent {
    pub faction_id: u32,
    pub season: Season,
    pub focal: (i32, i32),
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

/// Convert a single tile to `Road` and emit a `TileChangedEvent`. Used by
/// the door-finalization paths for the doormat tile (which sits at the
/// Bresenham `from` endpoint and is therefore skipped by `road_carve_system`).
/// Only writes when the current tile kind is a writable surface
/// (Grass / Scrub / Sand / soil-like); leaves Wall / Water / Stone alone.
/// Is the tile a valid doormat target — passable, not a wall/stone, not
/// blueprinted, not already-bed, not reserved as another door's doormat?
/// Used to gate door placement so a door's cardinal-out neighbour isn't a
/// neighbour's wall (which `write_road_tile` would refuse to overwrite,
/// leaving the door permanently blocked).
fn doormat_tile_clear(
    chunk_map: &ChunkMap,
    bed_map: &BedMap,
    bp_map: &BlueprintMap,
    doormat: &crate::simulation::doormat::DoormatReservations,
    tile: (i32, i32),
) -> bool {
    if bp_map.0.contains_key(&tile) || bed_map.0.contains_key(&tile) {
        return false;
    }
    if doormat.is_reserved(tile) {
        return false;
    }
    match chunk_map.tile_kind_at(tile.0, tile.1) {
        Some(k) if k == TileKind::Wall || k == TileKind::Stone => false,
        Some(k) if !k.is_passable() => false,
        Some(_) => true,
        None => false,
    }
}

/// Bounded BFS from `start` checking whether `home` is reachable via passable
/// terrain (Wall / Stone block; everything else is walkable). Treats existing
/// blueprints and beds as walkable so a freshly-staged hut doesn't trip itself
/// — only finalized walls block. Caps at `MAX_DOORMAT_BFS_STEPS` expansions
/// per call so the placement path stays cheap even on Bronze-Age starts.
///
/// Used by `pick_clear_door_cardinal`: a cardinal whose doormat is locally
/// clear but enclosed in a sealed courtyard fails this check, forcing the
/// caller to try another cardinal or abort. The cap is generous enough to
/// cover even Bronze-Age city footprints (≤ 80 tile chebyshev) but small
/// enough that placement stays cheap.
/// Folded into the shared `placement_reachability` layer — no parallel
/// reachability implementation survives. Preserves the legacy contract:
/// `Wall` / unloaded chunks block, the `Stone`-aversion heuristic is kept so
/// door selection on rocky starts is byte-stable, and the budget matches the
/// old `MAX_DOORMAT_BFS_STEPS`. The step model is upgraded from a 2D
/// 4-connected BFS to the agent-faithful 3D check, so a doormat is accepted
/// iff a worker can genuinely walk it home (sealed courtyards still fail —
/// `passable_diagonal_step` rejects wall-corner pinches exactly as before).
fn doormat_reaches_home(chunk_map: &ChunkMap, start: (i32, i32), home: (i32, i32)) -> bool {
    use crate::simulation::placement_reachability as reach;
    if start == home {
        return true;
    }
    let stone_averse =
        |t: (i32, i32, i32)| chunk_map.tile_kind_at(t.0, t.1) == Some(TileKind::Stone);
    reach::path_exists(
        chunk_map,
        reach::resolve3(chunk_map, start),
        reach::resolve3(chunk_map, home),
        reach::ReachOpts::seed()
            .with_cap(reach::DOORMAT_MAX_EXPANSIONS)
            .with_blocked(&stone_averse),
    )
}

/// Pick a door cardinal whose entrance cell's cardinal-out neighbour is a
/// clear doormat target. Tries `preferred` first; if its doormat is blocked
/// (neighbour wall, blueprint, bed, palisade, reserved doormat, impassable),
/// scans the other three cardinals in order of resulting doormat → home
/// chebyshev distance. Returns `(edge, entrance_offset, doormat_tile)` or
/// `None` when *every* cardinal yields a blocked doormat — caller should
/// abort the build rather than place an unreachable door.
fn pick_clear_door_cardinal(
    chunk_map: &ChunkMap,
    bed_map: &BedMap,
    bp_map: &BlueprintMap,
    doormat: &crate::simulation::doormat::DoormatReservations,
    centre: (i32, i32),
    half_w: i32,
    half_h: i32,
    preferred: crate::simulation::land::TileEdge,
    home: (i32, i32),
) -> Option<(crate::simulation::land::TileEdge, (i32, i32), (i32, i32))> {
    pick_clear_door_cardinal_filtered(
        chunk_map,
        bed_map,
        bp_map,
        doormat,
        centre,
        half_w,
        half_h,
        preferred,
        home,
        |_| false,
    )
}

fn pick_clear_door_cardinal_filtered<F>(
    chunk_map: &ChunkMap,
    bed_map: &BedMap,
    bp_map: &BlueprintMap,
    doormat: &crate::simulation::doormat::DoormatReservations,
    centre: (i32, i32),
    half_w: i32,
    half_h: i32,
    preferred: crate::simulation::land::TileEdge,
    home: (i32, i32),
    extra_blocked: F,
) -> Option<(crate::simulation::land::TileEdge, (i32, i32), (i32, i32))>
where
    F: Fn((i32, i32)) -> bool,
{
    use crate::simulation::land::TileEdge;
    let cardinals = [
        TileEdge::North,
        TileEdge::East,
        TileEdge::South,
        TileEdge::West,
    ];
    let try_edge = |e: TileEdge| -> Option<(TileEdge, (i32, i32), (i32, i32), i64)> {
        let entrance_offset = entrance_cell_for_edge(half_w, half_h, e, home, centre);
        let door_tile = (centre.0 + entrance_offset.0, centre.1 + entrance_offset.1);
        let (dx, dy) = e.delta();
        let dm = (door_tile.0 + dx, door_tile.1 + dy);
        if extra_blocked(dm) {
            return None;
        }
        if !doormat_tile_clear(chunk_map, bed_map, bp_map, doormat, dm) {
            return None;
        }
        // Reachability gate: the doormat must connect to the faction's home
        // tile through passable terrain. Sealed courtyards / pockets fail
        // here so the caller tries another cardinal.
        if !doormat_reaches_home(chunk_map, dm, home) {
            return None;
        }
        let d = ((dm.0 - home.0) as i64).pow(2) + ((dm.1 - home.1) as i64).pow(2);
        Some((e, entrance_offset, dm, d))
    };
    if let Some((e, off, dm, _)) = try_edge(preferred) {
        return Some((e, off, dm));
    }
    cardinals
        .iter()
        .copied()
        .filter(|&e| e != preferred)
        .filter_map(try_edge)
        .min_by_key(|&(_, _, _, d)| d)
        .map(|(e, off, dm, _)| (e, off, dm))
}

/// Is there any `TileKind::Road` tile within `radius` chebyshev of `from`?
/// Used to gate per-door road-carving so we don't pave the entire settlement
/// when many doors all push a fresh Bresenham line to home. The doormat tile
/// itself is always written; only the connection-to-home extension is gated.
fn road_within(chunk_map: &ChunkMap, from: (i32, i32), radius: i32) -> bool {
    for dy in -radius..=radius {
        for dx in -radius..=radius {
            if dx == 0 && dy == 0 {
                continue;
            }
            if chunk_map.tile_kind_at(from.0 + dx, from.1 + dy) == Some(TileKind::Road) {
                return true;
            }
        }
    }
    false
}

fn write_road_tile(
    chunk_map: &mut ChunkMap,
    tile_changed: &mut EventWriter<crate::world::chunk_streaming::TileChangedEvent>,
    tile: (i32, i32),
) {
    let cur = chunk_map.tile_kind_at(tile.0, tile.1);
    let writable = match cur {
        // Tilled farm soil is never paved — universal, streaming-safe guard
        // covering both the runtime and seed callers without plumbing.
        Some(TileKind::Cropland) => false,
        Some(TileKind::Grass) => true,
        Some(TileKind::Scrub) | Some(TileKind::Sand) => true,
        Some(k) if k.is_soil_like() => true,
        _ => false,
    };
    if !writable {
        return;
    }
    let surf_z = chunk_map.surface_z_at(tile.0, tile.1);
    chunk_map.set_tile(
        tile.0,
        tile.1,
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

/// Drains `RoadCarveQueue`. For each pending (faction_id, building_tile, home)
/// triple, walks a Bresenham line from the building back to the home tile and
/// converts each passable, non-Wall tile into `TileKind::Road`. Skips tiles
/// already road, blueprint, bed, wall, tilled `Cropland`, or otherwise
/// `tile_is_farm_protected` (inside an Agricultural plot / carrying a crop).
/// Emits `TileChangedEvent` for each converted tile so the renderer refreshes.
pub fn road_carve_system(
    mut queue: ResMut<RoadCarveQueue>,
    mut chunk_map: ResMut<ChunkMap>,
    bp_map: Res<BlueprintMap>,
    bed_map: Res<BedMap>,
    plot_index: Res<crate::simulation::land::PlotIndex>,
    plant_map: Res<crate::simulation::plants::PlantMap>,
    mut tile_changed: EventWriter<crate::world::chunk_streaming::TileChangedEvent>,
) {
    if queue.0.is_empty() {
        return;
    }
    // Drain — re-allocate a fresh empty Vec to release the lock on `queue`.
    let drained: Vec<(u32, (i32, i32), (i32, i32))> = std::mem::take(&mut queue.0);

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
                let tile = (x0 as i32, y0 as i32);
                if !bp_map.0.contains_key(&tile) && !bed_map.0.contains_key(&tile) {
                    let surf_z = chunk_map.surface_z_at(x0, y0);
                    let cur = chunk_map.tile_kind_at(x0, y0);
                    let writable = match cur {
                        // Never pave tilled fields (universal TileKind guard).
                        Some(TileKind::Cropland) => false,
                        Some(TileKind::Grass) => true,
                        Some(TileKind::Scrub) | Some(TileKind::Sand) => true,
                        Some(k) if k.is_soil_like() => true,
                        _ => false,
                    };
                    // Plus the runtime chokepoint guard: every RoadCarveQueue
                    // producer (doormat extension, spine drain, desire path,
                    // survey) drains here, so guarding this one site protects
                    // Agricultural-plot tiles and standing crops even before
                    // they are stamped `Cropland`.
                    if writable
                        && !crate::simulation::land::tile_is_farm_protected(
                            &plot_index,
                            &plant_map,
                            tile,
                        )
                    {
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
    mut agent_query: Query<(
        Entity,
        &mut PersonAI,
        &mut crate::simulation::typed_task::ActionQueue,
        &FactionMember,
        &Transform,
        &LodLevel,
    )>,
) {
    if clock.tick % UPGRADE_INTERVAL_TICKS != 0 {
        return;
    }

    // Snapshot all faction state we need (we'll mutate `active_upgrade` later).
    let faction_state: Vec<(
        u32,
        (i32, i32),
        FactionTechs,
        bool,
        bool,
        AHashMap<crate::economy::resource_catalog::ResourceId, u32>,
    )> = registry
        .factions
        .iter()
        .filter(|(&id, _)| id != SOLO)
        .map(|(&id, f)| {
            // Target wall material comes from the one poster-pool
            // surface (`buildable_techs`) — same as `chief_directive_
            // system`. A chief who only *heard* of bronze (Aware, not
            // Learned) doesn't trigger village-wide wall upgrades.
            (
                id,
                f.home_tile,
                f.buildable_techs,
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

        // Wall-upgrade pass: a deliberate chief tier-bump. Pass `None`
        // (unconstrained) — treasury procurement is out of scope here and
        // would surprise-drain on every upgrade tick. Identical to legacy.
        let target_mat = select_wall_material(&techs, None)
            .mat()
            .unwrap_or_else(|| best_wall_material(&techs));
        let target_rank = target_mat as u8;

        // Find one outdated wall within radius 25 of home.
        let (hx, hy) = (home.0 as i32, home.1 as i32);
        let mut outdated: Option<(i32, i32)> = None;
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
            .all(|&(rid, qty)| storage.get(&rid).copied().unwrap_or(0) >= (qty as u32) * 2);
        if !has_stock {
            continue;
        }

        // Find the closest idle, non-dormant faction member.
        let mut nearest: Option<(Entity, i32, (i32, i32))> = None;
        for (e, ai, aq, member, transform, lod) in agent_query.iter() {
            if member.faction_id != faction_id {
                continue;
            }
            if *lod == LodLevel::Dormant {
                continue;
            }
            if ai.state != AiState::Idle || aq.current_task_kind() != UNEMPLOYED_TASK_KIND {
                continue;
            }
            let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
            let ty = (transform.translation.y / TILE_SIZE).floor() as i32;
            let d = (tx - tile.0 as i32).abs() + (ty - tile.1 as i32).abs();
            if nearest.map(|(_, nd, _)| d < nd).unwrap_or(true) {
                nearest = Some((e, d, (tx as i32, ty as i32)));
            }
        }
        let Some((agent_e, _, cur_tile)) = nearest else {
            continue;
        };

        // Assign the Deconstruct task at the wall's tile.
        if let Ok((_, mut ai, mut aq, _, _, _)) = agent_query.get_mut(agent_e) {
            let cur_chunk = ChunkCoord(
                (cur_tile.0 as i32).div_euclid(CHUNK_SIZE as i32),
                (cur_tile.1 as i32).div_euclid(CHUNK_SIZE as i32),
            );
            let routed = assign_task_with_routing(
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
            if routed {
                aq.dispatch(crate::simulation::typed_task::Task::Deconstruct { tile });
            }
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
    mut doormat_reservations: ResMut<crate::simulation::doormat::DoormatReservations>,
    spatial: Res<crate::world::spatial::SpatialIndex>,
    mut tile_changed: EventWriter<crate::world::chunk_streaming::TileChangedEvent>,
    mut job_board: ResMut<JobBoard>,
    mut job_completed: EventWriter<JobCompletedEvent>,
    mut activity_log: EventWriter<crate::ui::activity_log::ActivityLogEvent>,
    mut bp_query: Query<&mut Blueprint>,
    member_query: Query<&FactionMember>,
    mut agent_query: Query<(
        Entity,
        &mut PersonAI,
        &mut crate::simulation::typed_task::ActionQueue,
        &mut EconomicAgent,
        &mut crate::simulation::carry::Carrier,
        &mut Skills,
        &BucketSlot,
        &LodLevel,
        Option<&JobClaim>,
    )>,
) {
    // Pass 1: collect pending contributions from Working agents, classified by
    // role.
    //   bp_haulers: bp_entity → Vec<(agent, inventory snapshot per deposit slot, claim)>
    //   bp_workers: bp_entity → Vec<agent>
    let mut bp_haulers: AHashMap<Entity, Vec<(Entity, [u32; MAX_BUILD_INPUTS], Option<JobClaim>)>> =
        AHashMap::new();
    let mut bp_workers: AHashMap<Entity, Vec<Entity>> = AHashMap::new();

    for (entity, mut ai, mut aq, agent, carrier, _skills, slot, lod, claim_opt) in
        agent_query.iter_mut()
    {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if ai.state != AiState::Working {
            continue;
        }
        let task = aq.current_task_kind();
        let is_hauler = task == TaskKind::HaulMaterials as u16;
        let is_worker = task == TaskKind::Construct as u16 || task == TaskKind::ConstructBed as u16;
        if !is_hauler && !is_worker {
            continue;
        }

        // Phase 3c-ii: workers read the blueprint from the typed
        // `Task::Construct` / `Task::ConstructBed` variant; haulers still
        // use `target_entity` (HaulMaterials hasn't migrated yet). Falls
        // through to `target_entity` for workers when the typed task is
        // absent so legacy dispatch paths that haven't been migrated still
        // work.
        let bp_entity_opt = if is_worker {
            aq.current
                .as_construct()
                .or_else(|| aq.current.as_construct_bed())
                .or(ai.target_entity)
        } else {
            ai.target_entity
        };
        let Some(bp_entity) = bp_entity_opt else {
            ai.state = AiState::Idle;
            ai.work_progress = 0;
            aq.advance();
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
            ai.work_progress = 0;
            ai.target_entity = None;
            aq.advance();
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
                    let id = deposits[i].resource_id;
                    let in_hand = carrier.quantity_of_resource(id);
                    let in_inv = agent.quantity_of_resource(id);
                    snap[i] = in_hand.saturating_add(in_inv);
                    if snap[i] > 0 {
                        useful = true;
                    }
                }
            }
            if !useful {
                // Nothing to drop here — release back to plan so it can re-route.
                // `aq.cancel_chain` drains the typed `Task::HaulToBlueprint`
                // (and any prefetched tail) so the next dispatcher tick re-plans
                // cleanly. Without it the stale haul task stacks until overflow.
                aq.cancel_chain(&mut ai);
                ai.target_entity = None;
                // Fix 2: also drop the Haul JobClaim if it points to this
                // satisfied bp. Without this, `job_claim_system` would re-claim
                // the same hauler against the same Haul posting on the next
                // tick, trapping them in a withdraw-walk-noop loop. Only drop
                // when the held claim is actually a Haul against THIS bp —
                // a Haul claim against a different bp is unrelated and stays.
                if let Some(claim) = claim_opt {
                    if claim.kind == JobKind::Haul {
                        let claim_bp_matches = job_board.get(claim.job_id).map_or(false, |p| {
                            matches!(
                                &p.progress,
                                JobProgress::Haul { blueprint, .. } if *blueprint == bp_entity
                            )
                        });
                        if claim_bp_matches {
                            commands.entity(entity).remove::<JobClaim>();
                            commands.entity(entity).remove::<ClaimTarget>();
                            release_claimant(&mut job_board, claim.job_id, entity);
                        }
                    }
                }
                continue;
            }
            bp_haulers
                .entry(bp_entity)
                .or_default()
                .push((entity, snap, claim_opt.copied()));
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
    // Workers waiting at an unsatisfied bp — clear their stale work_progress
    // counter (Fix 5). Pass 3 zeroes these.
    let mut work_progress_resets: Vec<Entity> = Vec::new();
    // Workers who made real build progress on an unfinished blueprint. Pass 3
    // yields them after a bounded slice so maintenance can run before they
    // resume the same preserved claim.
    let mut slice_candidates: Vec<Entity> = Vec::new();
    // (agent_entity, good, qty_to_remove)
    let mut good_removals: Vec<(Entity, crate::economy::resource_catalog::ResourceId, u32)> =
        Vec::new();

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
                orphaned_agents.extend(haulers.iter().map(|(e, _, _)| *e));
            }
            if let Some(workers) = bp_workers.get(&bp_entity) {
                orphaned_agents.extend(workers.iter().copied());
            }
            continue;
        };

        // Deposit hauler goods first. Credit any held Haul JobClaim with the
        // delivered quantity so the posting tracks completion.
        // Track which (resource_id) slots became satisfied this pass so we can
        // drop the matching Haul postings eagerly (Fix 1a) — without this,
        // postings whose `delivered` counter never matched `target` (because
        // claimants were dropped mid-trip and credits stopped) would linger
        // and trap fresh haulers in a withdraw-walk-noop loop.
        let bp_faction_id = bp.faction_id;
        let mut newly_satisfied_resources: Vec<crate::economy::resource_catalog::ResourceId> =
            Vec::with_capacity(MAX_BUILD_INPUTS);
        if let Some(haulers) = bp_haulers.get(&bp_entity) {
            for (agent_e, snap, claim_opt) in haulers {
                for i in 0..bp.deposit_count as usize {
                    let need = bp.deposits[i];
                    let still = need.needed.saturating_sub(need.deposited) as u32;
                    if still == 0 || snap[i] == 0 {
                        continue;
                    }
                    // Cap `take` to fit the u8 deposit counter (Fix 4). `still`
                    // is already ≤ u8::MAX today (both `needed` and `deposited`
                    // are u8), but capping again defends future recipes.
                    let take = still.min(snap[i]).min(u8::MAX as u32);
                    good_removals.push((*agent_e, need.resource_id, take));
                    let prev = bp.deposits[i].deposited;
                    bp.deposits[i].deposited = prev.saturating_add(take as u8);
                    let now_satisfied = prev < bp.deposits[i].needed
                        && bp.deposits[i].deposited >= bp.deposits[i].needed;
                    if now_satisfied && !newly_satisfied_resources.contains(&need.resource_id) {
                        newly_satisfied_resources.push(need.resource_id);
                    }
                    if let Some(claim) = claim_opt {
                        if claim.kind == JobKind::Haul {
                            record_progress_filtered(
                                &mut commands,
                                &mut job_board,
                                &mut job_completed,
                                claim,
                                JobKind::Haul,
                                Some(need.resource_id),
                                take,
                            );
                        }
                    }
                }
                hauler_done.push(*agent_e);
            }
        }
        // Fix 1a: drop any Haul posting whose (blueprint, resource_id) slot
        // just filled. Mirrors the cleanup pattern in `job_claim_release_system`
        // (jobs.rs ~line 2548): remove the posting, strip `JobClaim` +
        // `ClaimTarget` from every claimant, fire `JobCompletedEvent`. Without
        // this, claimants whose contributions weren't credited via
        // `record_progress_filtered` (e.g., crisis-goal preempt dropped their
        // claim mid-trip) would re-cycle through Withdraw→walk→noop forever.
        for satisfied_rid in &newly_satisfied_resources {
            let postings = job_board.faction_postings_mut(bp_faction_id);
            let mut idx = 0;
            while idx < postings.len() {
                let drop = matches!(
                    &postings[idx].progress,
                    JobProgress::Haul { blueprint, resource_id, .. }
                        if *blueprint == bp_entity && *resource_id == *satisfied_rid
                );
                if drop {
                    let dropped = postings.swap_remove(idx);
                    let claimants = dropped.claimants.clone();
                    for c in &claimants {
                        commands.entity(*c).remove::<JobClaim>();
                        commands.entity(*c).remove::<ClaimTarget>();
                    }
                    // Phase 0: Haul-slot filled = genuine completion.
                    job_completed.send(JobCompletedEvent {
                        job_id: dropped.id,
                        faction_id: bp_faction_id,
                        kind: dropped.kind,
                        claimants,
                        completed: true,
                        target_rid: dropped.progress.target_rid(),
                    });
                } else {
                    idx += 1;
                }
            }
        }

        // Advance work by one tick per on-site worker — but only once all
        // materials have been deposited AND every obstacle in the footprint
        // has been cleared. Gating on `is_satisfied()` + `obstacles_cleared()`
        // here (a) prevents `build_progress` from saturating past `work_ticks`
        // while haulers are still en route, and (b) keeps Building XP honest
        // by only awarding it for real labour. The obstacle gate makes
        // workers idle next to the blueprint until a `ClearObstacle` worker
        // (potentially themselves) cuts/relocates the last entry.
        let recipe = recipe_for(bp.kind);
        if bp.is_satisfied() && bp.obstacles_cleared() {
            if let Some(workers) = bp_workers.get(&bp_entity) {
                bp.build_progress = bp
                    .build_progress
                    .saturating_add(workers.len() as u8)
                    .min(recipe.work_ticks);
                xp_grants.extend(workers.iter().copied());
                if bp.build_progress < recipe.work_ticks {
                    slice_candidates.extend(workers.iter().copied());
                }
            }
        } else if let Some(workers) = bp_workers.get(&bp_entity) {
            // Fix 5: workers on-site at an unsatisfied bp accumulate dead
            // `ai.work_progress` from `movement_system`'s Working tick. The
            // counter isn't read by `construction_system` (`bp.build_progress`
            // is the real one), but it leaks into the inspector. Reset so the
            // displayed value stays meaningful.
            work_progress_resets.extend(workers.iter().copied());
        }

        if bp.build_progress >= recipe.work_ticks && bp.is_satisfied() && bp.obstacles_cleared() {
            let tile = bp.tile;
            let (tx, ty) = (tile.0 as i32, tile.1 as i32);

            let world_pos = tile_to_world(tx, ty);
            // sleepy-dove: tier picks read the poster's frozen
            // `design_techs` snapshot so a build started under one chief
            // (or architect) finalizes at the design tier even across
            // succession / poster death. Author-less blueprints
            // (`posted_by == None` — bridge / nomad emitters) fall back
            // to the faction's live `buildable_techs` (the same
            // poster-pool surface, just not snapshotted). Both branches
            // are the one consistent system — no community adoption.
            let build_techs = if bp.posted_by.is_some() {
                bp.design_techs
            } else {
                registry
                    .factions
                    .get(&bp.faction_id)
                    .map(|f| f.buildable_techs)
                    .unwrap_or_default()
            };
            let result_entity: Entity = match bp.kind {
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
                            ore: 0,
                        },
                    );

                    let wall_entity = commands
                        .spawn((
                            Wall { material },
                            StructureLabel(material.label()),
                            Transform::from_xyz(world_pos.x, world_pos.y, 0.4),
                            GlobalTransform::default(),
                            Visibility::Visible,
                            InheritedVisibility::default(),
                        ))
                        .id();
                    maps.wall_map.0.insert(tile, wall_entity);
                    wall_entity
                }
                BuildSiteKind::Bed => {
                    let bed = Bed {
                        owner: None,
                        tier: best_bed_for(&build_techs),
                    };
                    let label = bed.tier.label();
                    let bed_entity = commands
                        .spawn((
                            bed,
                            StructureLabel(label),
                            Transform::from_xyz(world_pos.x, world_pos.y, 0.35),
                            GlobalTransform::default(),
                            Visibility::Visible,
                            InheritedVisibility::default(),
                            crate::world::spatial::Indexed::new(
                                crate::world::spatial::IndexedKind::Bed,
                            ),
                        ))
                        .id();
                    maps.bed_map.0.insert(tile, bed_entity);
                    bed_entity
                }
                BuildSiteKind::Bedroll => {
                    // Nomadic Bed: same `Bed { tier: Crude }` semantics so
                    // sleep dispatch (`SleepMethod` / `BedMap`) finds it
                    // unchanged; the `Deployable` marker lets Phase 8's
                    // `pack_deployable` convert it back into a `bedroll`
                    // good when the camp moves.
                    let bed = Bed::default();
                    let bed_entity = commands
                        .spawn((
                            bed,
                            crate::simulation::pack_deploy::Deployable::fully_packable(
                                crate::economy::core_ids::bedroll(),
                            ),
                            StructureLabel("Bedroll"),
                            Transform::from_xyz(world_pos.x, world_pos.y, 0.35),
                            GlobalTransform::default(),
                            Visibility::Visible,
                            InheritedVisibility::default(),
                            crate::world::spatial::Indexed::new(
                                crate::world::spatial::IndexedKind::Bed,
                            ),
                        ))
                        .id();
                    maps.bed_map.0.insert(tile, bed_entity);
                    bed_entity
                }
                BuildSiteKind::Tent => {
                    // Sticks-and-leaves shelter. Deployed-only — the
                    // `Deployable::refund_only(0.5, crate::economy::core_ids::wood(), 6)` marker tells the
                    // migration teardown to drop half the input wood as
                    // GroundItems. Tent does NOT carry a Bed; nomads sleep
                    // in Bedrolls underneath.
                    let e = commands
                        .spawn((
                            TentShelter {
                                tier: ShelterTier::Tent,
                            },
                            crate::simulation::pack_deploy::Deployable::refund_only(
                                0.5,
                                crate::economy::core_ids::wood(),
                                6,
                            ),
                            StructureLabel("Tent"),
                            Transform::from_xyz(world_pos.x, world_pos.y, 0.4),
                            GlobalTransform::default(),
                            Visibility::Visible,
                            InheritedVisibility::default(),
                        ))
                        .id();
                    e
                }
                BuildSiteKind::Yurt => {
                    // Felt-and-lattice packable shelter. On migration,
                    // packs into a `packed_yurt` good (handled by Phase 8
                    // via `Deployable::fully_packable`).
                    let e = commands
                        .spawn((
                            TentShelter {
                                tier: ShelterTier::Yurt,
                            },
                            crate::simulation::pack_deploy::Deployable::fully_packable(
                                crate::economy::core_ids::packed_yurt(),
                            ),
                            StructureLabel("Yurt"),
                            Transform::from_xyz(world_pos.x, world_pos.y, 0.4),
                            GlobalTransform::default(),
                            Visibility::Visible,
                            InheritedVisibility::default(),
                        ))
                        .id();
                    e
                }
                BuildSiteKind::Campfire => {
                    let campfire = Campfire {
                        tier: best_hearth_for(&build_techs),
                    };
                    let label = campfire.tier.label();
                    let campfire_entity = commands
                        .spawn((
                            campfire,
                            StructureLabel(label),
                            Transform::from_xyz(world_pos.x, world_pos.y, 0.3),
                            GlobalTransform::default(),
                            Visibility::Visible,
                            InheritedVisibility::default(),
                        ))
                        .id();
                    maps.campfire_map.0.insert(tile, campfire_entity);
                    campfire_entity
                }
                BuildSiteKind::Door => {
                    // A door does NOT write a Wall tile — the underlying
                    // terrain stays passable. The Door entity carries the
                    // open/closed state consulted by line_of_sight. Door
                    // direction is sourced from `bp.door_dir` (frontage edge
                    // when plot-driven). `plan_building` always stamps a
                    // direction; an absent `door_dir` here means a Single-tile
                    // Door blueprint was placed without one — log + default to
                    // East so we don't silently break the doormat invariant.
                    let door_edge = bp.door_dir.unwrap_or_else(|| {
                        warn!(
                            "Door blueprint at {:?} had no door_dir; defaulting to East",
                            tile
                        );
                        crate::simulation::land::TileEdge::East
                    });
                    let (ddx, ddy) = door_edge.delta();
                    let doormat_tile = (tile.0 + ddx, tile.1 + ddy);
                    let door = Door {
                        faction_id: bp.faction_id,
                        open: false,
                        tier: best_door_for(&build_techs),
                        dir: door_edge,
                        doormat_tile,
                    };
                    let label = door.tier.label();
                    let door_entity = commands
                        .spawn((
                            door,
                            StructureLabel(label),
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
                    doormat_reservations.0.insert(
                        doormat_tile,
                        crate::simulation::doormat::DoormatEntry {
                            owner_door: door_entity,
                            door_tile: tile,
                            dir: door_edge,
                        },
                    );
                    // Carve doormat to Road directly; road_carve_system
                    // skips both endpoints. Extend toward the faction's home
                    // tile *only* when no existing road sits within 4 chebyshev
                    // of the doormat — otherwise the new door already connects
                    // naturally to the road network and a fresh Bresenham would
                    // just pave duplicate spokes.
                    write_road_tile(&mut *chunk_map, &mut tile_changed, doormat_tile);
                    if !road_within(&chunk_map, doormat_tile, 4) {
                        if let Some(faction) = registry.factions.get(&bp.faction_id) {
                            road_carve_queue.0.push((
                                bp.faction_id,
                                doormat_tile,
                                faction.home_tile,
                            ));
                        }
                    }
                    door_entity
                }
                BuildSiteKind::Workbench => {
                    let wb = Workbench {
                        faction_id: bp.faction_id,
                        tier: best_workbench_for(&build_techs),
                    };
                    let label = wb.tier.label();
                    let e = commands
                        .spawn((
                            wb,
                            StructureLabel(label),
                            crate::simulation::capital::OwnedBy {
                                faction_id: bp.faction_id,
                                kind: crate::simulation::capital::WorkshopKind::Workbench,
                                tile,
                            },
                            Transform::from_xyz(world_pos.x, world_pos.y, 0.35),
                            GlobalTransform::default(),
                            Visibility::Visible,
                            InheritedVisibility::default(),
                        ))
                        .id();
                    maps.workbench_map.0.insert(tile, e);
                    e
                }
                BuildSiteKind::Loom => {
                    let e = commands
                        .spawn((
                            Loom {
                                faction_id: bp.faction_id,
                            },
                            StructureLabel(BuildSiteKind::Loom.label()),
                            crate::simulation::capital::OwnedBy {
                                faction_id: bp.faction_id,
                                kind: crate::simulation::capital::WorkshopKind::Loom,
                                tile,
                            },
                            Transform::from_xyz(world_pos.x, world_pos.y, 0.35),
                            GlobalTransform::default(),
                            Visibility::Visible,
                            InheritedVisibility::default(),
                        ))
                        .id();
                    maps.loom_map.0.insert(tile, e);
                    e
                }
                BuildSiteKind::Table => {
                    let e = commands
                        .spawn((
                            Table,
                            StructureLabel(BuildSiteKind::Table.label()),
                            Transform::from_xyz(world_pos.x, world_pos.y, 0.35),
                            GlobalTransform::default(),
                            Visibility::Visible,
                            InheritedVisibility::default(),
                        ))
                        .id();
                    maps.table_map.0.insert(tile, e);
                    e
                }
                BuildSiteKind::Chair => {
                    let e = commands
                        .spawn((
                            Chair,
                            StructureLabel(BuildSiteKind::Chair.label()),
                            Transform::from_xyz(world_pos.x, world_pos.y, 0.35),
                            GlobalTransform::default(),
                            Visibility::Visible,
                            InheritedVisibility::default(),
                        ))
                        .id();
                    maps.chair_map.0.insert(tile, e);
                    e
                }
                BuildSiteKind::Granary => {
                    let e = commands
                        .spawn((
                            Granary {
                                faction_id: bp.faction_id,
                            },
                            StructureLabel(BuildSiteKind::Granary.label()),
                            crate::simulation::capital::OwnedBy {
                                faction_id: bp.faction_id,
                                kind: crate::simulation::capital::WorkshopKind::Granary,
                                tile,
                            },
                            Transform::from_xyz(world_pos.x, world_pos.y, 0.4),
                            GlobalTransform::default(),
                            Visibility::Visible,
                            InheritedVisibility::default(),
                        ))
                        .id();
                    maps.granary_map.0.insert(tile, e);
                    e
                }
                BuildSiteKind::Shrine => {
                    let e = commands
                        .spawn((
                            Shrine {
                                faction_id: bp.faction_id,
                            },
                            StructureLabel(BuildSiteKind::Shrine.label()),
                            crate::simulation::capital::OwnedBy {
                                faction_id: bp.faction_id,
                                kind: crate::simulation::capital::WorkshopKind::Shrine,
                                tile,
                            },
                            Transform::from_xyz(world_pos.x, world_pos.y, 0.4),
                            GlobalTransform::default(),
                            Visibility::Visible,
                            InheritedVisibility::default(),
                        ))
                        .id();
                    maps.shrine_map.0.insert(tile, e);
                    e
                }
                BuildSiteKind::Market => {
                    let e = commands
                        .spawn((
                            Market {
                                faction_id: bp.faction_id,
                            },
                            StructureLabel(BuildSiteKind::Market.label()),
                            crate::simulation::capital::OwnedBy {
                                faction_id: bp.faction_id,
                                kind: crate::simulation::capital::WorkshopKind::Market,
                                tile,
                            },
                            Transform::from_xyz(world_pos.x, world_pos.y, 0.4),
                            GlobalTransform::default(),
                            Visibility::Visible,
                            InheritedVisibility::default(),
                        ))
                        .id();
                    maps.market_map.0.insert(tile, e);
                    e
                }
                BuildSiteKind::Barracks => {
                    let e = commands
                        .spawn((
                            Barracks {
                                faction_id: bp.faction_id,
                            },
                            StructureLabel(BuildSiteKind::Barracks.label()),
                            crate::simulation::capital::OwnedBy {
                                faction_id: bp.faction_id,
                                kind: crate::simulation::capital::WorkshopKind::Barracks,
                                tile,
                            },
                            Transform::from_xyz(world_pos.x, world_pos.y, 0.4),
                            GlobalTransform::default(),
                            Visibility::Visible,
                            InheritedVisibility::default(),
                        ))
                        .id();
                    maps.barracks_map.0.insert(tile, e);
                    e
                }
                BuildSiteKind::Monument => {
                    let e = commands
                        .spawn((
                            Monument {
                                faction_id: bp.faction_id,
                            },
                            StructureLabel(BuildSiteKind::Monument.label()),
                            crate::simulation::capital::OwnedBy {
                                faction_id: bp.faction_id,
                                kind: crate::simulation::capital::WorkshopKind::Monument,
                                tile,
                            },
                            Transform::from_xyz(world_pos.x, world_pos.y, 0.45),
                            GlobalTransform::default(),
                            Visibility::Visible,
                            InheritedVisibility::default(),
                        ))
                        .id();
                    maps.monument_map.0.insert(tile, e);
                    e
                }
                BuildSiteKind::Latrine => commands
                    .spawn((
                        crate::simulation::sanitation::Latrine,
                        StructureLabel(BuildSiteKind::Latrine.label()),
                        Transform::from_xyz(world_pos.x, world_pos.y, 0.35),
                        GlobalTransform::default(),
                        Visibility::Visible,
                        InheritedVisibility::default(),
                    ))
                    .id(),
                BuildSiteKind::Bridge => {
                    // Tile-replacing finalize: stash the prior tile (River
                    // in the current build path), then rewrite to Bridge.
                    // The downstream `TileChangedEvent` triggers chunk-graph
                    // rebuild so pathfinding picks up the new road-speed cell.
                    let prior = chunk_map.tile_kind_at(tx, ty).unwrap_or(TileKind::River);
                    let surf_z = bp.target_z as i32;
                    chunk_map.set_tile(
                        tx,
                        ty,
                        surf_z,
                        TileData {
                            kind: TileKind::Bridge,
                            elevation: 0,
                            fertility: 0,
                            flags: 0b0001,
                            ore: 0,
                        },
                    );
                    let bridge_entity = commands
                        .spawn((
                            Bridge {
                                faction_id: bp.faction_id,
                                tile,
                                restore_tile: prior,
                            },
                            StructureLabel(BuildSiteKind::Bridge.label()),
                            Transform::from_xyz(world_pos.x, world_pos.y, 0.30),
                            GlobalTransform::default(),
                            Visibility::Visible,
                            InheritedVisibility::default(),
                        ))
                        .id();
                    maps.bridge_map.0.insert(tile, bridge_entity);
                    bridge_entity
                }
                BuildSiteKind::Dam => {
                    // Tile-replacing finalize, mirroring Bridge. The `Dam`
                    // entity is the durable truth; `TileKind::Dam` is its
                    // cache projection (restamped from `DamMap` on reload by
                    // `restamp_runtime_water_on_chunk_load`). The crest
                    // barrier is registered in `RuntimeWater` so the Phase 5
                    // fluid sim pools water upstream to it.
                    let prior = chunk_map.tile_kind_at(tx, ty).unwrap_or(TileKind::River);
                    let crest_z = bp.target_z;
                    chunk_map.set_tile(
                        tx,
                        ty,
                        crest_z as i32,
                        TileData {
                            kind: TileKind::Dam,
                            elevation: 0,
                            fertility: 0,
                            flags: 0b0001,
                            ore: 0,
                        },
                    );
                    maps.runtime_water.register_dam(tile, crest_z);
                    let dam_entity = commands
                        .spawn((
                            Dam {
                                faction_id: bp.faction_id,
                                tile,
                                restore_tile: prior,
                                crest_z,
                            },
                            StructureLabel(BuildSiteKind::Dam.label()),
                            Transform::from_xyz(world_pos.x, world_pos.y, 0.30),
                            GlobalTransform::default(),
                            Visibility::Visible,
                            InheritedVisibility::default(),
                        ))
                        .id();
                    maps.dam_map.0.insert(tile, dam_entity);
                    dam_entity
                }
                BuildSiteKind::Well => {
                    let well_entity = commands
                        .spawn((
                            Well {
                                faction_id: bp.faction_id,
                            },
                            StructureLabel(BuildSiteKind::Well.label()),
                            Transform::from_xyz(world_pos.x, world_pos.y, 0.35),
                            GlobalTransform::default(),
                            Visibility::Visible,
                            InheritedVisibility::default(),
                        ))
                        .id();
                    maps.well_map.0.insert(tile, well_entity);
                    well_entity
                }
            };

            // Emit a TileChangedEvent so pathfinding caches (flow fields,
            // chunk graph) see the new wall/furniture and re-route.
            tile_changed.send(crate::world::chunk_streaming::TileChangedEvent {
                tx: tile.0,
                ty: tile.1,
            });

            bp_map.0.remove(&tile);

            // Phase 5 (knowledge-posted construction): diffuse adoption from
            // practice. Posted-by Some marks a runtime build whose poster
            // (chief or architect) actually exercised the gating techs; seed
            // emissions (`posted_by == None`) don't count — seeding is not
            // practice. Records every tech the design relied on (recipe gate
            // + tier-driving tech). Drives `derive_stage`'s recent-use signal.
            if bp.posted_by.is_some() {
                if let Some(faction) = registry.factions.get_mut(&bp.faction_id) {
                    let now = clock.tick as u32;
                    for tech in gating_techs_for_completed_blueprint(&bp) {
                        crate::simulation::technology_adoption::record_tech_use(faction, tech, now);
                    }
                }
            }

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

            let lead_actor = bp_workers
                .get(&bp_entity)
                .and_then(|v| v.first().copied())
                .or_else(|| {
                    bp_haulers
                        .get(&bp_entity)
                        .and_then(|v| v.first().map(|(e, _, _)| *e))
                });
            if let Some(actor) = lead_actor {
                let faction_id = member_query.get(actor).map(|m| m.faction_id).unwrap_or(0);
                activity_log.send(crate::ui::activity_log::ActivityLogEvent {
                    tick: clock.tick,
                    actor,
                    faction_id,
                    kind: crate::ui::activity_log::ActivityEntryKind::Constructed {
                        site: bp.kind,
                        tile,
                        result_entity,
                    },
                });
            }

            if let Some(workers) = bp_workers.get(&bp_entity) {
                completed_agents.extend(workers.iter().copied());
            }
            if let Some(haulers) = bp_haulers.get(&bp_entity) {
                completed_agents.extend(haulers.iter().map(|(e, _, _)| *e));
            }
        }
    }

    if good_removals.is_empty()
        && completed_agents.is_empty()
        && hauler_done.is_empty()
        && orphaned_agents.is_empty()
        && xp_grants.is_empty()
        && work_progress_resets.is_empty()
    {
        return;
    }

    // Pass 3: remove deposited goods from agents, grant Building XP to workers
    // whose labour actually advanced progress, and reset completed/hauler/orphaned agents.
    for (entity, mut ai, mut aq, mut agent, mut carrier, mut skills, _, _, _) in
        agent_query.iter_mut()
    {
        for &(ae, id, qty) in &good_removals {
            if ae == entity {
                // Consume from hands first (where haulers typically carry the load),
                // fall back to personal inventory.
                let from_hand = carrier.remove_resource(id, qty);
                let still = qty - from_hand;
                if still > 0 {
                    agent.remove_resource(id, still);
                }
            }
        }

        if xp_grants.contains(&entity) {
            skills.gain_xp(SkillKind::Building, 1);
        }

        if work_progress_resets.contains(&entity) {
            ai.work_progress = 0;
        }

        let is_completed = completed_agents.contains(&entity);
        let is_hauler_done = hauler_done.contains(&entity);
        let is_orphaned = orphaned_agents.contains(&entity);

        if is_completed || is_hauler_done || is_orphaned {
            ai.state = AiState::Idle;
            ai.target_entity = None;
            ai.work_progress = 0;
            aq.advance();
        } else if slice_candidates.contains(&entity)
            && ai.work_progress >= MAINTENANCE_WORK_SLICE_TICKS
        {
            yield_for_maintenance_boundary(&mut ai, &mut aq);
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
    let bed_pos_by_entity: AHashMap<Entity, (i32, i32)> =
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
     -> Option<(i32, i32)> {
        let mut best: Option<((i32, i32), i8)> = None;
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
        let mut candidate: Option<(Entity, (i32, i32))> = None;
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
            let tx = tx_i32 as i32;
            let ty = ty_i32 as i32;
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
        partner_bed: Option<(i32, i32)>,
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
        let mut available: Vec<(Entity, (i32, i32))> = bed_map
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
        let tx = (transform.translation.x / crate::world::terrain::TILE_SIZE).floor() as i32;
        let ty = (transform.translation.y / crate::world::terrain::TILE_SIZE).floor() as i32;

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
        &mut crate::simulation::typed_task::ActionQueue,
        &mut EconomicAgent,
        &mut crate::simulation::carry::Carrier,
        &FactionMember,
        &Transform,
    )>,
    person_home_query: Query<(Entity, &HomeBed)>,
    wall_query: Query<&Wall>,
    bridge_query: Query<&Bridge>,
    dam_query: Query<&Dam>,
    storage_tile_map: Res<StorageTileMap>,
    chunk_graph: Res<ChunkGraph>,
    chunk_router: Res<ChunkRouter>,
    chunk_connectivity: Res<ChunkConnectivity>,
    mut chunk_map: ResMut<ChunkMap>,
    mut tile_changed: EventWriter<crate::world::chunk_streaming::TileChangedEvent>,
) {
    // Collect agents that just finished deconstruction.
    let mut to_complete: Vec<(Entity, (i32, i32), u32, (i32, i32))> = Vec::new();

    for (entity, mut ai, aq, _, _, member, transform) in agent_query.iter_mut() {
        if ai.state != AiState::Working || aq.current_task_kind() != TaskKind::Deconstruct as u16 {
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
        } else if let Some(e) = maps.bridge_map.0.remove(&tile) {
            removed = Some((e, BuildSiteKind::Bridge, false));
        } else if let Some(e) = maps.dam_map.0.remove(&tile) {
            removed = Some((e, BuildSiteKind::Dam, false));
        } else if let Some(e) = maps.well_map.0.remove(&tile) {
            removed = Some((e, BuildSiteKind::Well, false));
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
            // Already gone — clear the typed `Task::Deconstruct` so the next
            // tick's dispatcher sees a clean Idle slot.
            if let Ok((_, mut ai, mut aq, _, _, _, _)) = agent_query.get_mut(agent_entity) {
                aq.finish_task(&mut ai);
            }
            continue;
        };

        // Water-anchored structures (Bridge, Dam) restore the prior water
        // tile; the drop site is the nearest passable bank tile (refunds
        // dropped at the now-impassable water cell would be unrecoverable).
        // Dam additionally clears its `RuntimeWater` crest barrier so the
        // impounded water drains on the next Phase 5 solve.
        let water_anchored_refund_tile: Option<(i32, i32)> = if matches!(
            kind,
            BuildSiteKind::Bridge | BuildSiteKind::Dam
        ) {
            let restore = match kind {
                BuildSiteKind::Bridge => bridge_query
                    .get(target_entity)
                    .map(|b| b.restore_tile)
                    .unwrap_or(TileKind::River),
                BuildSiteKind::Dam => {
                    maps.runtime_water.clear_dam(tile);
                    dam_query
                        .get(target_entity)
                        .map(|d| d.restore_tile)
                        .unwrap_or(TileKind::River)
                }
                _ => unreachable!(),
            };
            let surf_z = chunk_map.surface_z_at(tile.0 as i32, tile.1 as i32);
            chunk_map.set_tile(
                tile.0 as i32,
                tile.1 as i32,
                surf_z as i32,
                TileData {
                    kind: restore,
                    ..Default::default()
                },
            );
            nearest_passable_bank(&chunk_map, (tile.0 as i32, tile.1 as i32))
        } else {
            None
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

        if let Ok((_, mut ai, mut aq, mut economic_agent, mut carrier, _, _)) =
            agent_query.get_mut(agent_entity)
        {
            // Recovered materials prefer the agent's hands so they can be hauled to
            // storage; fall back to inventory; spill any remainder at the deconstructed
            // tile as a GroundItem.
            let mut hand_qty: u32 = 0;
            let mut first_refund_rid: Option<crate::economy::resource_catalog::ResourceId> = None;
            for &(rid, qty) in &recipe_for(kind).deconstruct_refund {
                let qty = qty as u32;
                if first_refund_rid.is_none() {
                    first_refund_rid = Some(rid);
                }
                let item = crate::economy::item::Item::new_commodity(rid);
                let after_hand = carrier.try_pick_up(item, qty);
                hand_qty = hand_qty.saturating_add(qty.saturating_sub(after_hand));
                let after_inv = if after_hand > 0 {
                    economic_agent.add_item(item, after_hand)
                } else {
                    0
                };
                if after_inv > 0 {
                    let (dx, dy) =
                        water_anchored_refund_tile.unwrap_or((tile.0 as i32, tile.1 as i32));
                    let pos = tile_to_world(dx, dy);
                    commands.spawn((
                        crate::simulation::items::GroundItem {
                            item,
                            qty: after_inv,
                        },
                        Transform::from_xyz(pos.x, pos.y, 0.3),
                        GlobalTransform::default(),
                        Visibility::Visible,
                        InheritedVisibility::default(),
                        crate::world::spatial::Indexed::new(
                            crate::world::spatial::IndexedKind::GroundItem,
                        ),
                    ));
                }
            }

            let cur_tile = (cur_x as i32, cur_y as i32);
            let cur_chunk = ChunkCoord(
                cur_x.div_euclid(CHUNK_SIZE as i32),
                cur_y.div_euclid(CHUNK_SIZE as i32),
            );

            // Exit the typed `Task::Deconstruct` slot. If a refund landed in
            // hands and we have a reachable faction storage tile, queue a
            // `Task::DepositToFactionStorage` and route the agent so
            // `drop_items_at_destination_system` picks up on arrival. Mirrors
            // the canonical handoff in `gather::finish_gather`.
            aq.advance();

            let storage = if hand_qty > 0 {
                storage_tile_map.nearest_for_faction(faction_id, (tile.0 as i32, tile.1 as i32))
            } else {
                None
            };

            if let Some(storage_tile) = storage {
                let rid = first_refund_rid.unwrap_or_else(crate::economy::core_ids::wood);
                aq.dispatch(
                    crate::simulation::typed_task::Task::DepositToFactionStorage {
                        resource_id: rid,
                        target_faction_id: None,
                    },
                );
                let dispatched = assign_task_with_routing(
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
                if !dispatched {
                    aq.cancel_chain(&mut ai);
                }
            } else {
                ai.state = AiState::Idle;
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

// ── Game-start building seeding ───────────────────────────────────────────────
//
// Places era-appropriate, fully-built structures around each faction's home
// tile when the game starts. Skips the blueprint pipeline entirely — agents
// don't gather materials, the structures are simply spawned and registered
// in their respective tile maps.
//
// Per-era layout (additive — each era adds to all prior eras' structures):
//   Paleolithic: 1 Open Campfire, 4 Crude Beds
//   Mesolithic:  +2 Beds (6 total)
//   Neolithic:   Hearth tier→Lined; +Granary, +Workbench, +Loom, beds→8
//   Chalcolithic: +Shrine, +Palisade ring with one Door (radius 4)
//   Bronze Age:  +Market, +Barracks, +Monument; walls upgrade to Mudbrick
//
// Placement uses simple radial spirals from the home tile, gating each tile
// on `chunk_map.is_passable`. The chief planner's per-kind counters
// (`count_beds_near`, `count_campfires_near`, …) are radius-based, so
// structures placed within radius 6 of home register correctly and the
// chief won't re-queue them as blueprints.

/// Iterate tiles outward from `home` in increasing chebyshev rings, skipping
/// any tile that's already in `used` or fails `keep`. Yields up to
/// `MAX_PLACEMENT_ATTEMPTS` candidates before giving up.
pub(crate) fn next_clear_tile(
    home: (i32, i32),
    used: &AHashSet<(i32, i32)>,
    chunk_map: &ChunkMap,
    max_radius: i32,
) -> Option<(i32, i32)> {
    let (hx, hy) = home;
    for ring in 1..=max_radius {
        for dy in -ring..=ring {
            for dx in -ring..=ring {
                if dx.abs().max(dy.abs()) != ring {
                    continue;
                }
                let pos = (hx + dx, hy + dy);
                if used.contains(&pos) {
                    continue;
                }
                if !chunk_map.is_passable(pos.0, pos.1) {
                    continue;
                }
                let Some(k) = chunk_map.tile_kind_at(pos.0, pos.1) else {
                    continue;
                };
                if k == TileKind::Wall || k == TileKind::Stone {
                    continue;
                }
                return Some(pos);
            }
        }
    }
    None
}

/// Stamp a single-tile structure at an explicit tile (caller has already
/// resolved placement via plan / brain / seed loop). Mirrors the body of
/// `spawn_seeded_structure` but skips the radial `pick_seed_tile` search.
/// Used by the seed-mode intent resolver so candidates produced by
/// `generate_candidates` land at the chosen tile.
fn spawn_seeded_structure_at_tile(
    commands: &mut Commands,
    maps: &mut FurnitureMaps,
    _chunk_map: &mut ChunkMap,
    tile_changed: &mut EventWriter<crate::world::chunk_streaming::TileChangedEvent>,
    used: &mut AHashSet<(i32, i32)>,
    tile: (i32, i32),
    faction_id: u32,
    kind: BuildSiteKind,
    seed_techs: &FactionTechs,
) {
    used.insert(tile);
    let world_pos = tile_to_world(tile.0, tile.1);

    match kind {
        BuildSiteKind::Bed => {
            let bed = Bed {
                owner: None,
                tier: best_bed_for(seed_techs),
            };
            let label = bed.tier.label();
            let e = commands
                .spawn((
                    bed,
                    StructureLabel(label),
                    Transform::from_xyz(world_pos.x, world_pos.y, 0.35),
                    GlobalTransform::default(),
                    Visibility::Visible,
                    InheritedVisibility::default(),
                    crate::world::spatial::Indexed::new(crate::world::spatial::IndexedKind::Bed),
                ))
                .id();
            maps.bed_map.0.insert(tile, e);
        }
        BuildSiteKind::Bedroll => {
            let bed = Bed::default();
            let e = commands
                .spawn((
                    bed,
                    crate::simulation::pack_deploy::Deployable::fully_packable(
                        crate::economy::core_ids::bedroll(),
                    ),
                    StructureLabel("Bedroll"),
                    Transform::from_xyz(world_pos.x, world_pos.y, 0.35),
                    GlobalTransform::default(),
                    Visibility::Visible,
                    InheritedVisibility::default(),
                    crate::world::spatial::Indexed::new(crate::world::spatial::IndexedKind::Bed),
                ))
                .id();
            maps.bed_map.0.insert(tile, e);
        }
        BuildSiteKind::Tent => {
            commands.spawn((
                TentShelter {
                    tier: ShelterTier::Tent,
                },
                crate::simulation::pack_deploy::Deployable::refund_only(
                    0.5,
                    crate::economy::core_ids::wood(),
                    6,
                ),
                StructureLabel("Tent"),
                Transform::from_xyz(world_pos.x, world_pos.y, 0.4),
                GlobalTransform::default(),
                Visibility::Visible,
                InheritedVisibility::default(),
            ));
        }
        BuildSiteKind::Yurt => {
            commands.spawn((
                TentShelter {
                    tier: ShelterTier::Yurt,
                },
                crate::simulation::pack_deploy::Deployable::fully_packable(
                    crate::economy::core_ids::packed_yurt(),
                ),
                StructureLabel("Yurt"),
                Transform::from_xyz(world_pos.x, world_pos.y, 0.4),
                GlobalTransform::default(),
                Visibility::Visible,
                InheritedVisibility::default(),
            ));
        }
        BuildSiteKind::Campfire => {
            let campfire = Campfire {
                tier: best_hearth_for(seed_techs),
            };
            let label = campfire.tier.label();
            let e = commands
                .spawn((
                    campfire,
                    StructureLabel(label),
                    Transform::from_xyz(world_pos.x, world_pos.y, 0.3),
                    GlobalTransform::default(),
                    Visibility::Visible,
                    InheritedVisibility::default(),
                ))
                .id();
            maps.campfire_map.0.insert(tile, e);
        }
        BuildSiteKind::Workbench => {
            let wb = Workbench {
                faction_id,
                tier: best_workbench_for(seed_techs),
            };
            let label = wb.tier.label();
            let e = commands
                .spawn((
                    wb,
                    StructureLabel(label),
                    crate::simulation::capital::OwnedBy {
                        faction_id,
                        kind: crate::simulation::capital::WorkshopKind::Workbench,
                        tile,
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
                    Loom { faction_id },
                    StructureLabel(BuildSiteKind::Loom.label()),
                    crate::simulation::capital::OwnedBy {
                        faction_id,
                        kind: crate::simulation::capital::WorkshopKind::Loom,
                        tile,
                    },
                    Transform::from_xyz(world_pos.x, world_pos.y, 0.35),
                    GlobalTransform::default(),
                    Visibility::Visible,
                    InheritedVisibility::default(),
                ))
                .id();
            maps.loom_map.0.insert(tile, e);
        }
        BuildSiteKind::Granary => {
            let e = commands
                .spawn((
                    Granary { faction_id },
                    StructureLabel(BuildSiteKind::Granary.label()),
                    crate::simulation::capital::OwnedBy {
                        faction_id,
                        kind: crate::simulation::capital::WorkshopKind::Granary,
                        tile,
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
                    Shrine { faction_id },
                    StructureLabel(BuildSiteKind::Shrine.label()),
                    crate::simulation::capital::OwnedBy {
                        faction_id,
                        kind: crate::simulation::capital::WorkshopKind::Shrine,
                        tile,
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
                    Market { faction_id },
                    StructureLabel(BuildSiteKind::Market.label()),
                    crate::simulation::capital::OwnedBy {
                        faction_id,
                        kind: crate::simulation::capital::WorkshopKind::Market,
                        tile,
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
                    Barracks { faction_id },
                    StructureLabel(BuildSiteKind::Barracks.label()),
                    crate::simulation::capital::OwnedBy {
                        faction_id,
                        kind: crate::simulation::capital::WorkshopKind::Barracks,
                        tile,
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
                    Monument { faction_id },
                    StructureLabel(BuildSiteKind::Monument.label()),
                    crate::simulation::capital::OwnedBy {
                        faction_id,
                        kind: crate::simulation::capital::WorkshopKind::Monument,
                        tile,
                    },
                    Transform::from_xyz(world_pos.x, world_pos.y, 0.45),
                    GlobalTransform::default(),
                    Visibility::Visible,
                    InheritedVisibility::default(),
                ))
                .id();
            maps.monument_map.0.insert(tile, e);
        }
        BuildSiteKind::Well => {
            let e = commands
                .spawn((
                    Well { faction_id },
                    StructureLabel(BuildSiteKind::Well.label()),
                    Transform::from_xyz(world_pos.x, world_pos.y, 0.35),
                    GlobalTransform::default(),
                    Visibility::Visible,
                    InheritedVisibility::default(),
                ))
                .id();
            maps.well_map.0.insert(tile, e);
        }
        // Wall and Door are placed by `seed_perimeter`, not here.
        _ => return,
    }

    tile_changed.send(crate::world::chunk_streaming::TileChangedEvent {
        tx: tile.0,
        ty: tile.1,
    });
}

/// Per-faction starting structures, dispatched on `OnEnter(GameState::Playing)`
/// after `spawn_population`. Reads `GameStartOptions::era` and
/// `GameStartOptions::seed_buildings` (sandbox mode disables seeding).
///
/// **Seed-vs-grow contract** (Construction Overhaul Phase 0): this system
/// defines the *initial conditions* of a game-start settlement. The civic
/// milestone table (Phase 5) gates *growth only* — it does not retroactively
/// validate seeded buildings. So a Bronze-era starting settlement may seed
/// `Market`/`Barracks`/`Monument` even at low founding population; those
/// structures are grandfathered. Subsequent civic-building decisions go
/// through `chief_directive_system` and obey the milestone table.
pub fn seed_starting_buildings_system(
    mut commands: Commands,
    mut chunk_map: ResMut<ChunkMap>,
    mut maps: FurnitureMaps,
    mut tile_changed: EventWriter<crate::world::chunk_streaming::TileChangedEvent>,
    registry: Res<FactionRegistry>,
    options: Res<crate::GameStartOptions>,
    mut doormat_reservations: ResMut<crate::simulation::doormat::DoormatReservations>,
    mut road_carve_queue: ResMut<RoadCarveQueue>,
    brains: Res<crate::simulation::organic_settlement::SettlementBrains>,
    settlement_map: Res<crate::simulation::settlement::SettlementMap>,
    plot_index: Res<crate::simulation::land::PlotIndex>,
    plot_q: Query<&crate::simulation::land::Plot>,
    settlement_q: Query<&crate::simulation::settlement::Settlement>,
) {
    if !options.seed_buildings {
        return;
    }

    let era = options.era;
    // sleepy-dove Phase 7: the seed driver is now a typed profile, not a
    // raw `techs_through_era` bitset masquerading as adoption state. The
    // bitset is an implementation detail threaded through the shared
    // seed pipeline via `profile.seed_techs()`.
    let profile = SeedConstructionProfile::from_era(era);
    let seed_techs = *profile.seed_techs();
    let hearth_tier = profile.hearth_tier;

    // Generate-candidates needs an empty bp_map at seed time; we don't spawn
    // any blueprints in seed mode, so the chief's pending-blueprint accounting
    // is always zero. Likewise pending_kinds is empty.
    let empty_bp_map = BlueprintMap::default();
    let empty_pending: AHashMap<BuildSiteKind, u32> = AHashMap::new();

    for (&faction_id, faction) in registry.factions.iter() {
        if faction_id == SOLO || faction.member_count == 0 {
            continue;
        }
        let home = faction.home_tile;
        let members = faction.member_count;
        let mut used: AHashSet<(i32, i32)> = AHashSet::new();
        // Reserve the home tile itself — settled factions place their
        // FactionStorageTile here; nomadic factions still want it free of
        // structure overlaps so the camp anchor stays clear.
        used.insert(home);

        // ── Nomadic camps short-circuit the era ladder ─────────────────
        // Camps still use the nomad-specific seeder (deployable shelters,
        // packable yurts) which the unified intent loop doesn't model.
        if faction.caps.settlement.is_camp() {
            seed_nomadic_camp(
                &mut commands,
                &mut maps,
                &chunk_map,
                &mut tile_changed,
                &mut used,
                faction_id,
                home,
                members,
                era,
                hearth_tier,
            );
            continue;
        }

        // Settlement plan for the candidate generator. Prefer the brain-derived
        // compat plan (one zone per parcel — matches what the runtime chief
        // sees) and fall back to the deterministic `build_settlement_plan`
        // when no brain exists yet (sandbox / camp / first-tick edge cases).
        let brain_ref = settlement_map
            .by_faction
            .get(&faction_id)
            .and_then(|ids| ids.first().copied())
            .and_then(|sid| brains.0.get(&sid));
        let plan = if let Some(b) = brain_ref {
            crate::simulation::organic_settlement::compat_plan_from_brain(faction_id, faction, 0, b)
        } else {
            crate::simulation::settlement::build_settlement_plan(faction_id, faction, 0)
        };
        let plan_ref = Some(&plan);

        // Peak population for civic-milestone gates. At seed time this falls
        // back to current member_count (the Settlement entity hasn't yet
        // ratcheted peak via `settlement_peak_population_system`).
        let peak_pop = settlement_map
            .first_for_faction(faction_id)
            .and_then(|sid| settlement_map.by_id.get(&sid))
            .and_then(|&e| settlement_q.get(e).ok())
            .map(|s| s.peak_population)
            .unwrap_or(faction.member_count);

        // Branch on era. Paleo / Meso keep the band-camp seeder
        // (`paleolithic_hearth_positions_river_aware` provides the canonical
        // multi-hearth layout that the unified candidate loop doesn't
        // reproduce — its bootstrap path only fires when no plan exists).
        // Neo+ runs through the unified `generate_candidates` loop so seed
        // and runtime emit the same intent stream.
        if (era as u8) < (Era::Neolithic as u8) {
            // Per-era founder count: house every member with a Paleo / Meso
            // floor so very-small bands still get a basic camp.
            let era_min: u32 = match era {
                Era::Paleolithic => 4,
                _ => 6,
            };
            let target_beds: u32 = members.max(era_min);
            let hearth_positions =
                crate::simulation::settlement::paleolithic_hearth_positions_river_aware(
                    &chunk_map, faction_id, home, members,
                );
            let n_hearths = hearth_positions.len().max(1) as u32;
            let beds_per_hearth = (target_beds + n_hearths - 1) / n_hearths;
            for &offset_pos in hearth_positions.iter() {
                let hearth_tile = if !used.contains(&offset_pos)
                    && chunk_map.is_passable(offset_pos.0, offset_pos.1)
                {
                    offset_pos
                } else if let Some(t) = next_clear_tile(offset_pos, &used, &chunk_map, 4) {
                    t
                } else {
                    continue;
                };
                used.insert(hearth_tile);
                let world_pos = tile_to_world(hearth_tile.0, hearth_tile.1);
                let e = commands
                    .spawn((
                        Campfire { tier: hearth_tier },
                        StructureLabel(hearth_tier.label()),
                        Transform::from_xyz(world_pos.x, world_pos.y, 0.3),
                        GlobalTransform::default(),
                        Visibility::Visible,
                        InheritedVisibility::default(),
                    ))
                    .id();
                maps.campfire_map.0.insert(hearth_tile, e);
                tile_changed.send(crate::world::chunk_streaming::TileChangedEvent {
                    tx: hearth_tile.0,
                    ty: hearth_tile.1,
                });
                seed_paleo_beds_around_hearth(
                    &mut commands,
                    &mut maps,
                    &chunk_map,
                    &mut tile_changed,
                    &mut used,
                    hearth_tile,
                    beds_per_hearth,
                );
            }
            continue;
        }

        // ── Neolithic+: unified intent loop ────────────────────────────
        // Drives `generate_candidates` (the runtime chief's selector)
        // until it stops returning candidates or the safety cap fires.
        // Stamps each chosen intent via `seed_apply_intent`; the next
        // iteration sees the updated bed / hearth / civic / wall counts
        // and gates accordingly. One pipeline, two consumers.
        //
        // Cap reasoning: a Bronze 80-pop start needs ~80 huts + 1 of each
        // civic + ~48 palisade tiles ≈ 130 candidates. 512 is comfortable
        // headroom against any future intent kind.
        const MAX_SEED_ITERATIONS: u32 = 512;
        let mut last_progress_iter: u32 = 0;
        for iter in 0..MAX_SEED_ITERATIONS {
            let mut candidates = generate_candidates(
                faction_id,
                faction,
                plan_ref,
                &chunk_map,
                &maps.as_view(),
                &empty_bp_map,
                &doormat_reservations,
                &empty_pending,
                &plot_index,
                &plot_q,
                peak_pop,
                Some(&seed_techs),
                // Ignored in seed mode (seed_techs is Some); pass the
                // same era profile for clarity.
                seed_techs,
            );
            if candidates.is_empty() {
                break;
            }
            candidates.sort_by(|a, b| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });

            let mut applied_any = false;
            for best in candidates {
                let intent = best.intent;
                let tile = best.tile;
                let door_dir = best.door_dir;
                let applied_tile = seed_apply_intent(
                    &mut commands,
                    &mut maps,
                    &mut chunk_map,
                    &mut tile_changed,
                    &mut used,
                    &mut doormat_reservations,
                    &mut road_carve_queue,
                    faction_id,
                    home,
                    intent,
                    tile,
                    door_dir,
                    &seed_techs,
                );
                if let Some(applied_tile) = applied_tile {
                    applied_any = true;
                    last_progress_iter = iter;
                    // Farmstead yard for residential footprints. Yards bump
                    // fertility on adjacent tiles and reserve them in `used`
                    // so the next iteration's footprint search avoids them.
                    let (half_w, half_h) = match intent {
                        BuildIntent::Hut(_) => (1, 1),
                        BuildIntent::Longhouse(_) => (2, 1),
                        _ => (0, 0),
                    };
                    if half_w > 0 {
                        let yard_side = if (era as u8) >= (Era::BronzeAge as u8) {
                            3
                        } else {
                            2
                        };
                        seed_farmstead_yard(
                            &mut chunk_map,
                            &mut tile_changed,
                            &mut used,
                            &doormat_reservations,
                            home,
                            applied_tile.0,
                            applied_tile.1,
                            half_w,
                            half_h,
                            yard_side,
                            yard_side,
                        );
                    }
                    break;
                }
                // Mark tile as used so lower-priority candidates in this pass
                // and the next generate_candidates call can make progress away
                // from a blocked anchor.
                used.insert(tile);
            }

            if !applied_any {
                // Stall guard: 32 consecutive no-progress iterations means
                // generate_candidates is stuck on blocked anchors with no
                // path forward. Bail.
                if iter.saturating_sub(last_progress_iter) > 32 {
                    break;
                }
            }
        }
    }
}

/// Place a fully-built walled house at `(cx, cy)` with `half_w × half_h`
/// footprint, mirroring `plan_building`'s perimeter+entrance+interior-beds
/// shape but bypassing the Blueprint pipeline. Used by the seeder so
/// Neolithic+ founders start in their own house instead of a bare bed.
///
/// Returns `true` if the building was placed, `false` if any tile in the
/// footprint was unsuitable (impassable, already used, existing wall).
fn seed_walled_house_at(
    commands: &mut Commands,
    maps: &mut FurnitureMaps,
    chunk_map: &mut ChunkMap,
    tile_changed: &mut EventWriter<crate::world::chunk_streaming::TileChangedEvent>,
    used: &mut AHashSet<(i32, i32)>,
    cx: i32,
    cy: i32,
    half_w: i32,
    half_h: i32,
    interior_beds: &[(i32, i32)],
    wall_material: WallMaterial,
    faction_id: u32,
    home: (i32, i32),
    door_dir: Option<crate::simulation::land::TileEdge>,
    doormat: &mut crate::simulation::doormat::DoormatReservations,
    road_carve: &mut RoadCarveQueue,
    seed_techs: &FactionTechs,
) -> bool {
    // Pre-flight: every tile in the footprint must be clear (passable, not a
    // wall/stone/road, not already used, not reserved as another building's
    // doormat). Roads are protected so a seeded hut can't pave over an existing
    // street carved by the spine or a neighbour's doormat extension.
    for dy in -half_h..=half_h {
        for dx in -half_w..=half_w {
            let tile = (cx + dx, cy + dy);
            if used.contains(&tile) {
                return false;
            }
            if doormat.is_reserved(tile) {
                return false;
            }
            if !chunk_map.is_passable(tile.0, tile.1) {
                return false;
            }
            let Some(k) = chunk_map.tile_kind_at(tile.0, tile.1) else {
                return false;
            };
            if k == TileKind::Wall || k == TileKind::Stone || k == TileKind::Road {
                return false;
            }
        }
    }

    // Entrance: centre cell of the chosen frontage cardinal (or fall back to
    // cardinal-toward-home). If that cardinal's doormat is blocked by a
    // neighbour wall / blueprint / reserved doormat / impassable terrain, pick
    // the next-best cardinal whose doormat IS clear. Abort the build entirely
    // if no cardinal works — placing an unreachable door is strictly worse
    // than placing nothing.
    let preferred_edge =
        door_dir.unwrap_or_else(|| crate::simulation::land::TileEdge::toward((cx, cy), home));
    let Some((door_edge, entrance, planned_doormat)) = pick_clear_door_cardinal_filtered(
        chunk_map,
        &maps.bed_map,
        // seeded path has no blueprint map at hand — pass an empty one. The
        // game-start seeder operates on a fresh world, so blueprints don't
        // exist yet; only `used` and the chunk's tile_kind matter.
        &BlueprintMap::default(),
        doormat,
        (cx, cy),
        half_w,
        half_h,
        preferred_edge,
        home,
        |tile| used.contains(&tile),
    ) else {
        return false;
    };

    // Stamp tiles using the shared wall+door+bed plan (same enumeration the
    // runtime blueprint path uses). The seed path resolves each plan entry by
    // writing tiles directly into `chunk_map` instead of spawning Blueprints.
    let plan = walled_house_tile_plan(
        cx,
        cy,
        half_w,
        half_h,
        entrance,
        door_edge,
        wall_material,
        interior_beds,
    );

    // Simulated-build reachability gate (runs before any wall is stamped, so
    // it sees pre-stamp terrain + the planned wall overlay): doormat connects
    // home and every interior bed is reachable through the door. Refuse the
    // anchor rather than seed a house with a sealed bed.
    if !plan_reachable_from_home(chunk_map, home, planned_doormat, &plan) {
        return false;
    }

    for (kind, tile, edge) in &plan {
        let tile = *tile;
        let world_pos = tile_to_world(tile.0, tile.1);
        match kind {
            BuildSiteKind::Door => {
                let door_edge = edge.expect("door entry carries its edge");
                let (ddx, ddy) = door_edge.delta();
                let doormat_tile = (tile.0 + ddx, tile.1 + ddy);
                let door = Door {
                    faction_id,
                    open: false,
                    tier: best_door_for(seed_techs),
                    dir: door_edge,
                    doormat_tile,
                };
                let label = door.tier.label();
                let e = commands
                    .spawn((
                        door,
                        StructureLabel(label),
                        Transform::from_xyz(world_pos.x, world_pos.y, 0.4),
                        GlobalTransform::default(),
                        Visibility::Visible,
                        InheritedVisibility::default(),
                    ))
                    .id();
                maps.door_map.0.insert(
                    tile,
                    DoorEntry {
                        entity: e,
                        open: false,
                    },
                );
                doormat.0.insert(
                    doormat_tile,
                    crate::simulation::doormat::DoormatEntry {
                        owner_door: e,
                        door_tile: tile,
                        dir: door_edge,
                    },
                );
                // Carve doormat tile directly to Road; the Bresenham
                // road_carve_system skips both endpoints. Push an extension
                // from doormat → home so the door connects back to the spine.
                write_road_tile(&mut *chunk_map, tile_changed, doormat_tile);
                road_carve.0.push((faction_id, doormat_tile, home));
            }
            BuildSiteKind::Wall(mat) => {
                let surf_z = chunk_map.surface_z_at(tile.0, tile.1);
                chunk_map.set_tile(
                    tile.0,
                    tile.1,
                    surf_z + 1,
                    TileData {
                        kind: TileKind::Wall,
                        elevation: 0,
                        fertility: 0,
                        flags: 0b0001,
                        ore: 0,
                    },
                );
                let e = commands
                    .spawn((
                        Wall { material: *mat },
                        StructureLabel(mat.label()),
                        Transform::from_xyz(world_pos.x, world_pos.y, 0.4),
                        GlobalTransform::default(),
                        Visibility::Visible,
                        InheritedVisibility::default(),
                    ))
                    .id();
                maps.wall_map.0.insert(tile, e);
            }
            BuildSiteKind::Bed => {
                let bed = Bed {
                    owner: None,
                    tier: best_bed_for(seed_techs),
                };
                let label = bed.tier.label();
                let e = commands
                    .spawn((
                        bed,
                        StructureLabel(label),
                        Transform::from_xyz(world_pos.x, world_pos.y, 0.35),
                        GlobalTransform::default(),
                        Visibility::Visible,
                        InheritedVisibility::default(),
                        crate::world::spatial::Indexed::new(
                            crate::world::spatial::IndexedKind::Bed,
                        ),
                    ))
                    .id();
                maps.bed_map.0.insert(tile, e);
            }
            _ => {
                debug_assert!(
                    false,
                    "walled_house_tile_plan emitted unexpected BuildSiteKind {:?}",
                    kind
                );
            }
        }
        used.insert(tile);
        tile_changed.send(crate::world::chunk_streaming::TileChangedEvent {
            tx: tile.0,
            ty: tile.1,
        });
    }
    true
}

/// Stamp a `yard_w × yard_h` patch of tilled `Cropland` tiles adjacent to the
/// house centred at `(cx, cy)` with `half_w × half_h` footprint. The yard
/// extends out from the house's east wall by default; if east is blocked,
/// tries west, south, north in turn. Tile mutations skip Wall / Stone /
/// already-used tiles. Returns the count of tiles flipped.
///
/// This is the v1 "Farmstead" template (Neolithic+): house + attached
/// yard, mirroring the `FootprintShape::LShape` concept without yet
/// going through a full `BuildIntent::Composite` refactor. Phase 6's
/// `Plot.parent_plot` mechanism picks up these yards once a household
/// acquires the residential lot they sit on.
fn seed_farmstead_yard(
    chunk_map: &mut ChunkMap,
    tile_changed: &mut EventWriter<crate::world::chunk_streaming::TileChangedEvent>,
    used: &mut AHashSet<(i32, i32)>,
    doormat: &crate::simulation::doormat::DoormatReservations,
    home: (i32, i32),
    cx: i32,
    cy: i32,
    half_w: i32,
    half_h: i32,
    yard_w: i32,
    yard_h: i32,
) -> u32 {
    // Try directions in order: east, west, south, north. First side
    // where the full yard fits wins.
    let candidates: [(i32, i32, i32, i32); 4] = [
        // (yard_origin_x, yard_origin_y, dx_step, dy_step) — these are
        // bounding-box origin offsets from the house centre.
        (cx + half_w + 1, cy - yard_h / 2, 1, 1), // East
        (cx - half_w - yard_w, cy - yard_h / 2, 1, 1), // West
        (cx - yard_w / 2, cy + half_h + 1, 1, 1), // South
        (cx - yard_w / 2, cy - half_h - yard_h, 1, 1), // North
    ];

    let yard_clear = |x0: i32,
                      y0: i32,
                      used: &AHashSet<(i32, i32)>,
                      cm: &ChunkMap,
                      dm: &crate::simulation::doormat::DoormatReservations|
     -> bool {
        for ty in y0..y0 + yard_h {
            for tx in x0..x0 + yard_w {
                if used.contains(&(tx, ty)) {
                    return false;
                }
                if dm.is_reserved((tx, ty)) {
                    return false;
                }
                if !cm.is_passable(tx, ty) {
                    return false;
                }
                let Some(k) = cm.tile_kind_at(tx, ty) else {
                    return false;
                };
                if k == TileKind::Wall || k == TileKind::Stone || k.is_water_like() {
                    return false;
                }
            }
        }
        true
    };

    // The yard must also be genuinely walkable from `home` *given the house
    // walls that were just stamped* — otherwise the farmer can never reach it.
    let chosen = candidates.iter().find(|(x0, y0, _, _)| {
        yard_clear(*x0, *y0, used, chunk_map, doormat)
            && crate::simulation::placement_reachability::rect_reachable_from_home(
                chunk_map,
                home,
                (*x0, *y0),
                (*x0 + yard_w - 1, *y0 + yard_h - 1),
            )
    });
    let Some(&(x0, y0, _, _)) = chosen else {
        return 0;
    };

    // Yard tiles are tilled into visible `Cropland` and bumped to fertility
    // 200 (high-yield plot). `Cropland` is `is_soil_like`, so the soil-aware
    // planting heuristics still pick it, and road carving never paves it.
    let mut placed = 0u32;
    for ty in y0..y0 + yard_h {
        for tx in x0..x0 + yard_w {
            let z = chunk_map.surface_z_at(tx, ty);
            let cur = chunk_map.tile_at(tx, ty, z as i32);
            chunk_map.set_tile(
                tx,
                ty,
                z as i32,
                TileData {
                    kind: TileKind::Cropland,
                    elevation: cur.elevation,
                    fertility: 200,
                    flags: cur.flags,
                    ore: cur.ore,
                },
            );
            used.insert((tx, ty));
            tile_changed.send(crate::world::chunk_streaming::TileChangedEvent { tx, ty });
            placed += 1;
        }
    }
    placed
}

/// Place beds in a ring of radius 2..=4 around `hearth`, up to `count`.
/// Used by the Paleolithic/Mesolithic seeding path so each campfire gets
/// its own bed cluster.
/// Seed a nomadic band camp at `home`. No walls, no plots, no granaries:
/// hearths via the same `paleolithic_hearth_positions` ring layout the band
/// camp uses, then one Bedroll per founder around each hearth, plus 1 Tent
/// per 4 founders for shelter; Neolithic+ adds 1 Yurt per ~5 founders.
///
/// Mirrors `seed_paleo_beds_around_hearth`'s direct-spawn pattern (no
/// Blueprint pipeline) so the camp is fully built at game-start AND can be
/// re-invoked by `nomad_migration_commit_system` (Phase 8 follow-on) to
/// stand up a fresh camp at the new `home_tile`.
/// Bug-fix #6: returns the chebyshev radius around a seed home that
/// covers every entity `seed_nomadic_camp` will spawn. Outer-ring
/// tents fall at 5..=7 from each hearth, plus a few-tile safety
/// margin for offset hearth layouts and large-band hearth rings. For
/// 12-member bands → 12; for 24-member bands → 14; for 40+ → 18.
/// Callers (`pack_camp_assets`, `process_settlement_lifecycle_system`)
/// use this in place of the legacy `OLD_CAMP_RADIUS = 12` constant
/// when they have a member count.
pub fn seed_nomadic_camp_extent(members: u32, _era: Era) -> i32 {
    let base = 8_i32;
    let scale = (members as i32) / 6;
    (base + scale + 4).clamp(12, 22)
}

pub(crate) fn seed_nomadic_camp(
    commands: &mut Commands,
    maps: &mut FurnitureMaps,
    chunk_map: &ChunkMap,
    tile_changed: &mut EventWriter<crate::world::chunk_streaming::TileChangedEvent>,
    used: &mut AHashSet<(i32, i32)>,
    faction_id: u32,
    home: (i32, i32),
    members: u32,
    era: Era,
    hearth_tier: HearthTier,
) {
    let hearth_positions = crate::simulation::settlement::paleolithic_hearth_positions_river_aware(
        chunk_map, faction_id, home, members,
    );
    let n_hearths = hearth_positions.len().max(1) as u32;
    // Bedroll per founder, evenly split across hearths (round up).
    let bedrolls_per_hearth = (members.max(1) + n_hearths - 1) / n_hearths;
    // 1 tent per 4 founders, minimum 1 per camp.
    let tents_total = (members.max(1) + 3) / 4;
    let tents_per_hearth = tents_total.max(1).div_ceil(n_hearths).max(1);
    // Yurts: Neolithic+ only, 1 per ~5 members, capped at 2 per camp.
    let yurts_total = if (era as u8) >= (Era::Neolithic as u8) {
        (members.max(1) / 5).clamp(1, 2)
    } else {
        0
    };

    let mut yurts_remaining = yurts_total;

    for &offset_pos in hearth_positions.iter() {
        // Snap the hearth to a passable tile.
        let hearth_tile =
            if !used.contains(&offset_pos) && chunk_map.is_passable(offset_pos.0, offset_pos.1) {
                offset_pos
            } else if let Some(t) = next_clear_tile(offset_pos, used, chunk_map, 4) {
                t
            } else {
                continue;
            };
        used.insert(hearth_tile);
        let world_pos = tile_to_world(hearth_tile.0, hearth_tile.1);
        let e = commands
            .spawn((
                Campfire { tier: hearth_tier },
                StructureLabel(hearth_tier.label()),
                Transform::from_xyz(world_pos.x, world_pos.y, 0.3),
                GlobalTransform::default(),
                Visibility::Visible,
                InheritedVisibility::default(),
            ))
            .id();
        maps.campfire_map.0.insert(hearth_tile, e);
        tile_changed.send(crate::world::chunk_streaming::TileChangedEvent {
            tx: hearth_tile.0,
            ty: hearth_tile.1,
        });

        // Bedrolls — same crescent placement as paleo beds, but each entity
        // carries the `Deployable::fully_packable(bedroll)` marker so Phase 8
        // can pack them when the camp moves.
        seed_bedrolls_around_hearth(
            commands,
            maps,
            chunk_map,
            tile_changed,
            used,
            hearth_tile,
            bedrolls_per_hearth,
        );

        // Tents — outer ring shelter (sticks-and-leaves; 50% wood refund on
        // teardown via `Deployable::refund_only(0.5, crate::economy::core_ids::wood(), 6)`).
        seed_tents_around_hearth(
            commands,
            chunk_map,
            tile_changed,
            used,
            hearth_tile,
            tents_per_hearth,
        );

        // Yurts — packable felt shelter, only at Neolithic+. Distribute as
        // many as `yurts_remaining` allows; nomadic Bronze Age camps still
        // top out at 2 yurts per band.
        if yurts_remaining > 0 {
            let yurts_here = yurts_remaining.min(1);
            seed_yurts_around_hearth(
                commands,
                chunk_map,
                tile_changed,
                used,
                hearth_tile,
                yurts_here,
            );
            yurts_remaining = yurts_remaining.saturating_sub(yurts_here);
        }
    }
}

fn seed_bedrolls_around_hearth(
    commands: &mut Commands,
    maps: &mut FurnitureMaps,
    chunk_map: &ChunkMap,
    tile_changed: &mut EventWriter<crate::world::chunk_streaming::TileChangedEvent>,
    used: &mut AHashSet<(i32, i32)>,
    hearth: (i32, i32),
    count: u32,
) {
    let bedroll_id = crate::economy::core_ids::bedroll();
    let mut placed = 0u32;
    'outer: for radius in 2i32..=5 {
        for dy in -radius..=radius {
            for dx in -radius..=radius {
                if dx.abs().max(dy.abs()) != radius {
                    continue;
                }
                if placed >= count {
                    break 'outer;
                }
                let tile = (hearth.0 + dx, hearth.1 + dy);
                if used.contains(&tile) {
                    continue;
                }
                if !chunk_map.is_passable(tile.0, tile.1) {
                    continue;
                }
                let Some(k) = chunk_map.tile_kind_at(tile.0, tile.1) else {
                    continue;
                };
                if k == TileKind::Wall || k == TileKind::Stone {
                    continue;
                }
                let world_pos = tile_to_world(tile.0, tile.1);
                let bed = Bed::default();
                let e = commands
                    .spawn((
                        bed,
                        crate::simulation::pack_deploy::Deployable::fully_packable(bedroll_id),
                        StructureLabel("Bedroll"),
                        Transform::from_xyz(world_pos.x, world_pos.y, 0.35),
                        GlobalTransform::default(),
                        Visibility::Visible,
                        InheritedVisibility::default(),
                        crate::world::spatial::Indexed::new(
                            crate::world::spatial::IndexedKind::Bed,
                        ),
                    ))
                    .id();
                maps.bed_map.0.insert(tile, e);
                used.insert(tile);
                tile_changed.send(crate::world::chunk_streaming::TileChangedEvent {
                    tx: tile.0,
                    ty: tile.1,
                });
                placed += 1;
            }
        }
    }
}

fn seed_tents_around_hearth(
    commands: &mut Commands,
    chunk_map: &ChunkMap,
    tile_changed: &mut EventWriter<crate::world::chunk_streaming::TileChangedEvent>,
    used: &mut AHashSet<(i32, i32)>,
    hearth: (i32, i32),
    count: u32,
) {
    let mut placed = 0u32;
    // Outer ring (radius 5..=7) so tents shelter the bedrolls in 2..=5.
    'outer: for radius in 5i32..=7 {
        for dy in -radius..=radius {
            for dx in -radius..=radius {
                if dx.abs().max(dy.abs()) != radius {
                    continue;
                }
                if placed >= count {
                    break 'outer;
                }
                let tile = (hearth.0 + dx, hearth.1 + dy);
                if used.contains(&tile) {
                    continue;
                }
                if !chunk_map.is_passable(tile.0, tile.1) {
                    continue;
                }
                let Some(k) = chunk_map.tile_kind_at(tile.0, tile.1) else {
                    continue;
                };
                if k == TileKind::Wall || k == TileKind::Stone {
                    continue;
                }
                let world_pos = tile_to_world(tile.0, tile.1);
                commands.spawn((
                    TentShelter {
                        tier: ShelterTier::Tent,
                    },
                    crate::simulation::pack_deploy::Deployable::refund_only(
                        0.5,
                        crate::economy::core_ids::wood(),
                        6,
                    ),
                    StructureLabel("Tent"),
                    Transform::from_xyz(world_pos.x, world_pos.y, 0.4),
                    GlobalTransform::default(),
                    Visibility::Visible,
                    InheritedVisibility::default(),
                ));
                used.insert(tile);
                tile_changed.send(crate::world::chunk_streaming::TileChangedEvent {
                    tx: tile.0,
                    ty: tile.1,
                });
                placed += 1;
            }
        }
    }
}

fn seed_yurts_around_hearth(
    commands: &mut Commands,
    chunk_map: &ChunkMap,
    tile_changed: &mut EventWriter<crate::world::chunk_streaming::TileChangedEvent>,
    used: &mut AHashSet<(i32, i32)>,
    hearth: (i32, i32),
    count: u32,
) {
    let packed_yurt_id = crate::economy::core_ids::packed_yurt();
    let mut placed = 0u32;
    // Yurts go on the inner ring next to the hearth — they're the chief's
    // big shelter and the social anchor of the camp.
    'outer: for radius in 3i32..=5 {
        for dy in -radius..=radius {
            for dx in -radius..=radius {
                if dx.abs().max(dy.abs()) != radius {
                    continue;
                }
                if placed >= count {
                    break 'outer;
                }
                let tile = (hearth.0 + dx, hearth.1 + dy);
                if used.contains(&tile) {
                    continue;
                }
                if !chunk_map.is_passable(tile.0, tile.1) {
                    continue;
                }
                let Some(k) = chunk_map.tile_kind_at(tile.0, tile.1) else {
                    continue;
                };
                if k == TileKind::Wall || k == TileKind::Stone {
                    continue;
                }
                let world_pos = tile_to_world(tile.0, tile.1);
                commands.spawn((
                    TentShelter {
                        tier: ShelterTier::Yurt,
                    },
                    crate::simulation::pack_deploy::Deployable::fully_packable(packed_yurt_id),
                    StructureLabel("Yurt"),
                    Transform::from_xyz(world_pos.x, world_pos.y, 0.4),
                    GlobalTransform::default(),
                    Visibility::Visible,
                    InheritedVisibility::default(),
                ));
                used.insert(tile);
                tile_changed.send(crate::world::chunk_streaming::TileChangedEvent {
                    tx: tile.0,
                    ty: tile.1,
                });
                placed += 1;
            }
        }
    }
}

fn seed_paleo_beds_around_hearth(
    commands: &mut Commands,
    maps: &mut FurnitureMaps,
    chunk_map: &ChunkMap,
    tile_changed: &mut EventWriter<crate::world::chunk_streaming::TileChangedEvent>,
    used: &mut AHashSet<(i32, i32)>,
    hearth: (i32, i32),
    count: u32,
) {
    let mut placed = 0u32;
    'outer: for radius in 2i32..=5 {
        for dy in -radius..=radius {
            for dx in -radius..=radius {
                if dx.abs().max(dy.abs()) != radius {
                    continue;
                }
                if placed >= count {
                    break 'outer;
                }
                let tile = (hearth.0 + dx, hearth.1 + dy);
                if used.contains(&tile) {
                    continue;
                }
                if !chunk_map.is_passable(tile.0, tile.1) {
                    continue;
                }
                let Some(k) = chunk_map.tile_kind_at(tile.0, tile.1) else {
                    continue;
                };
                if k == TileKind::Wall || k == TileKind::Stone {
                    continue;
                }
                let world_pos = tile_to_world(tile.0, tile.1);
                let bed = Bed::default();
                let label = bed.tier.label();
                let e = commands
                    .spawn((
                        bed,
                        StructureLabel(label),
                        Transform::from_xyz(world_pos.x, world_pos.y, 0.35),
                        GlobalTransform::default(),
                        Visibility::Visible,
                        InheritedVisibility::default(),
                        crate::world::spatial::Indexed::new(
                            crate::world::spatial::IndexedKind::Bed,
                        ),
                    ))
                    .id();
                maps.bed_map.0.insert(tile, e);
                used.insert(tile);
                tile_changed.send(crate::world::chunk_streaming::TileChangedEvent {
                    tx: tile.0,
                    ty: tile.1,
                });
                placed += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::simulation::land::TileEdge;

    #[test]
    fn entrance_cell_picks_centre_of_chosen_side() {
        // Hut footprint: half_w=1, half_h=1 (3×3). Centre at (10, 10), home
        // toward east at (20, 10). East frontage → entrance at (+1, 0).
        let e = entrance_cell_for_edge(1, 1, TileEdge::East, (20, 10), (10, 10));
        assert_eq!(e, (1, 0));
        // West frontage → entrance at (-1, 0).
        let e = entrance_cell_for_edge(1, 1, TileEdge::West, (-5, 10), (10, 10));
        assert_eq!(e, (-1, 0));
        // North frontage → entrance at (0, +1).
        let e = entrance_cell_for_edge(1, 1, TileEdge::North, (10, 20), (10, 10));
        assert_eq!(e, (0, 1));
    }

    #[test]
    fn entrance_cell_never_corner_for_3x3() {
        for edge in [
            TileEdge::North,
            TileEdge::South,
            TileEdge::East,
            TileEdge::West,
        ] {
            let (dx, dy) = entrance_cell_for_edge(1, 1, edge, (10, 10), (10, 10));
            // For half_w=half_h=1 every entrance is on a flat side (one
            // coordinate ±1, the other 0).
            assert!(
                (dx.abs() == 1 && dy == 0) || (dx == 0 && dy.abs() == 1),
                "edge={:?} produced corner-cell offset ({dx}, {dy})",
                edge
            );
        }
    }

    #[test]
    fn bridge_recipe_gated_on_bridge_building() {
        let r = recipe_for(BuildSiteKind::Bridge);
        assert_eq!(r.tech_gate, Some(BRIDGE_BUILDING));
        assert!(!r.deconstruct_refund.is_empty());
    }

    #[test]
    fn well_digging_tech_def() {
        let def = &TECH_TREE[WELL_DIGGING as usize];
        assert_eq!(def.id, WELL_DIGGING);
        assert_eq!(def.era, Era::Neolithic);
        assert!(def.prerequisites.contains(&FLINT_KNAPPING));
        assert!(def.prerequisites.contains(&PERM_SETTLEMENT));
        assert_eq!(
            crate::simulation::technology_adoption::tech_scale(WELL_DIGGING),
            crate::simulation::technology_adoption::AdoptionScale::Institutional
        );
    }

    #[test]
    fn well_recipe_inputs_and_gate() {
        use crate::economy::core_ids;
        let _ = core_ids::catalog();
        let r = recipe_for(BuildSiteKind::Well);
        assert_eq!(r.tech_gate, Some(WELL_DIGGING));
        assert_eq!(r.work_ticks, 120);
        let stone = core_ids::stone();
        let wood = core_ids::wood();
        assert!(r.inputs.contains(&(stone, 4)));
        assert!(r.inputs.contains(&(wood, 2)));
        assert!(r.deconstruct_refund.contains(&(stone, 2)));
        assert!(r.deconstruct_refund.contains(&(wood, 1)));
    }

    #[test]
    fn faction_cannot_build_well_without_well_digging() {
        let techs = FactionTechs::default();
        assert!(!faction_can_build(BuildSiteKind::Well, &techs));
        let mut techs2 = FactionTechs::default();
        techs2.unlock(WELL_DIGGING);
        assert!(faction_can_build(BuildSiteKind::Well, &techs2));
    }

    #[test]
    fn bridge_kind_is_water_anchored() {
        assert!(BuildSiteKind::Bridge.is_water_anchored());
        assert!(!BuildSiteKind::Wall(WallMaterial::Palisade).is_water_anchored());
        assert!(!BuildSiteKind::Bed.is_water_anchored());
    }

    #[test]
    fn blueprint_worker_target_falls_back_to_anchor() {
        let mut bp = Blueprint::new(0, None, BuildSiteKind::Bed, (4, 5), 0);
        assert_eq!(bp.worker_target_tile(), (4, 5));
        bp.work_stand = Some((6, 5));
        assert_eq!(bp.worker_target_tile(), (6, 5));
    }

    #[test]
    fn walled_house_tile_plan_3x3_door_east() {
        // Hut footprint at (10,10), half=1, east entrance.
        let plan = walled_house_tile_plan(
            10,
            10,
            1,
            1,
            (1, 0),
            TileEdge::East,
            WallMaterial::WattleDaub,
            &[(0, 0)],
        );
        // 8 perimeter + 1 interior bed = 9 entries.
        assert_eq!(plan.len(), 9);
        // Exactly one Door cell, on the east side carrying its edge.
        let doors: Vec<_> = plan
            .iter()
            .filter(|(k, _, _)| matches!(k, BuildSiteKind::Door))
            .collect();
        assert_eq!(doors.len(), 1);
        assert_eq!(doors[0].1, (11, 10));
        assert_eq!(doors[0].2, Some(TileEdge::East));
        // Exactly 7 walls, none carrying an edge.
        let walls: Vec<_> = plan
            .iter()
            .filter(|(k, _, _)| matches!(k, BuildSiteKind::Wall(_)))
            .collect();
        assert_eq!(walls.len(), 7);
        assert!(walls.iter().all(|(_, _, e)| e.is_none()));
        // Exactly one bed at the interior.
        let beds: Vec<_> = plan
            .iter()
            .filter(|(k, _, _)| matches!(k, BuildSiteKind::Bed))
            .collect();
        assert_eq!(beds.len(), 1);
        assert_eq!(beds[0].1, (10, 10));
    }

    #[test]
    fn walled_house_tile_plan_propagates_wall_material() {
        // Both seed and runtime paths must stamp the requested material —
        // regression-guard the BuildSiteKind::Wall(mat) propagation.
        let plan = walled_house_tile_plan(
            0,
            0,
            1,
            1,
            (1, 0),
            TileEdge::East,
            WallMaterial::Mudbrick,
            &[],
        );
        let mats: Vec<_> = plan
            .iter()
            .filter_map(|(k, _, _)| match k {
                BuildSiteKind::Wall(m) => Some(*m),
                _ => None,
            })
            .collect();
        assert_eq!(mats.len(), 7);
        assert!(mats.iter().all(|m| *m == WallMaterial::Mudbrick));
    }

    #[test]
    fn walled_house_tile_plan_longhouse_door_on_long_side() {
        // Longhouse half_w=2, half_h=1 (5×3). East entrance at (+2, 0).
        // Two interior beds at offsets (-1,0) and (1,0).
        let plan = walled_house_tile_plan(
            0,
            0,
            2,
            1,
            (2, 0),
            TileEdge::East,
            WallMaterial::WattleDaub,
            &[(-1, 0), (1, 0)],
        );
        // Perimeter cells: 5*3 - (3*1 interior) = 15 - 3 = 12.
        // Plus 2 bed entries = 14.
        assert_eq!(plan.len(), 14);
        let door = plan
            .iter()
            .find(|(k, _, _)| matches!(k, BuildSiteKind::Door))
            .unwrap();
        assert_eq!(door.1, (2, 0));
        // Bed cells exactly where requested.
        let bed_tiles: Vec<_> = plan
            .iter()
            .filter_map(|(k, t, _)| match k {
                BuildSiteKind::Bed => Some(*t),
                _ => None,
            })
            .collect();
        assert_eq!(bed_tiles, vec![(-1, 0), (1, 0)]);
    }

    #[test]
    fn entrance_cell_longhouse_centres_along_long_side() {
        // Longhouse: half_w=2, half_h=1 (5×3). East frontage clamps dy=0 so
        // the entrance lands on the centre cell of the east edge.
        let e = entrance_cell_for_edge(2, 1, TileEdge::East, (20, 10), (10, 10));
        assert_eq!(e, (2, 0));
        // North frontage on a longhouse: clamp camp_home.0=10 → centre cell
        // gets dx=0.
        let e = entrance_cell_for_edge(2, 1, TileEdge::North, (10, 20), (10, 10));
        assert_eq!(e, (0, 1));
    }

    // ── sleepy-dove: poster-authorization primitives ─────────────────────

    #[test]
    fn poster_can_post_kind_gates_on_recipe_tech() {
        // Bed has no recipe tech_gate → any poster can post it (tier
        // falls back to Crude via best_bed_for).
        let none = FactionTechs::default();
        assert!(poster_can_post_kind(BuildSiteKind::Bed, &none));
        assert!(poster_can_post_kind(BuildSiteKind::Door, &none));
        // A Mudbrick wall's recipe IS gated. Empty knowledge can't post it;
        // a poster who Learned the gating tech can.
        let gate = recipe_for(BuildSiteKind::Wall(WallMaterial::Mudbrick)).tech_gate;
        if let Some(t) = gate {
            assert!(!poster_can_post_kind(
                BuildSiteKind::Wall(WallMaterial::Mudbrick),
                &none
            ));
            let mut learned = FactionTechs::default();
            learned.unlock(t);
            assert!(poster_can_post_kind(
                BuildSiteKind::Wall(WallMaterial::Mudbrick),
                &learned
            ));
        }
    }

    #[test]
    fn poster_can_post_intent_requires_all_parts() {
        // A Hut needs Wall + Door + Bed. With a gated wall material, a
        // poster lacking the wall tech can't author the whole Hut even
        // though Door/Bed are no-tech.
        let none = FactionTechs::default();
        if let Some(t) = recipe_for(BuildSiteKind::Wall(WallMaterial::Mudbrick)).tech_gate {
            assert!(!poster_can_post_intent(
                BuildIntent::Hut(WallMaterial::Mudbrick),
                &none
            ));
            let mut learned = FactionTechs::default();
            learned.unlock(t);
            assert!(poster_can_post_intent(
                BuildIntent::Hut(WallMaterial::Mudbrick),
                &learned
            ));
        }
        // A Palisade Hut is all no-tech → empty knowledge can author it.
        assert!(poster_can_post_intent(
            BuildIntent::Hut(WallMaterial::Palisade),
            &none
        ));
    }

    #[test]
    fn construction_relevant_techs_nonempty_and_includes_tier_drivers() {
        let v = construction_relevant_techs();
        assert!(v.contains(&FIRED_POTTERY));
        assert!(v.contains(&PERM_SETTLEMENT));
        // Sorted + deduped.
        let mut sorted = v.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(v, sorted);
    }

    #[test]
    fn seed_profile_reproduces_era_table() {
        // Paleo band-camp hearth must stay Open even though FLINT_KNAPPING
        // (a Paleolithic tech) would otherwise drive best_hearth_for to
        // Ringed. Guards the regression that motivated the explicit table.
        let paleo = SeedConstructionProfile::from_era(Era::Paleolithic);
        assert_eq!(paleo.hearth_tier, HearthTier::Open);
        assert!(paleo.wall_material.is_none());
        // Neolithic+ resolves tiers via the ladder and unlocks walls.
        let neo = SeedConstructionProfile::from_era(Era::Neolithic);
        assert!(neo.wall_material.is_some());
        let bronze = SeedConstructionProfile::from_era(Era::BronzeAge);
        assert_eq!(bronze.hearth_tier, HearthTier::Lined);
        // seed_techs() must match the legacy era derivation exactly.
        assert_eq!(bronze.seed_techs().0, techs_through_era(Era::BronzeAge).0);
    }

    #[test]
    fn poster_pool_union_and_select() {
        use crate::simulation::settlement::SettlementId;
        let mut pool = ConstructionPosterPool::default();
        let chief = Entity::from_raw(1);
        let architect = Entity::from_raw(2);
        let sid = SettlementId(7);
        // Chief knows nothing gated; architect knows FIRED_POTTERY.
        let chief_cap = PosterCapability {
            entity: chief,
            faction_id: 3,
            settlement_id: Some(sid),
            learned: FactionTechs::default(),
            building_skill: 10,
            social_skill: 5,
            class: ConstructionPosterClass::Chief,
        };
        let mut arch_learned = FactionTechs::default();
        arch_learned.unlock(FIRED_POTTERY);
        let arch_cap = PosterCapability {
            entity: architect,
            faction_id: 3,
            settlement_id: Some(sid),
            learned: arch_learned,
            building_skill: 50,
            social_skill: 2,
            class: ConstructionPosterClass::Architect,
        };
        pool.chief_by_faction.insert(3, chief_cap.clone());
        pool.by_settlement
            .insert((3, sid), vec![chief_cap, arch_cap]);

        // Union covers FIRED_POTTERY (from the architect).
        let u = pool.union_of_learned(3, Some(sid));
        assert!(u.has(FIRED_POTTERY));

        // A Mudbrick wall (FIRED_POTTERY-gated) resolves to the architect,
        // since the chief can't author it.
        let resolved = pool
            .select_poster_for_kind(3, Some(sid), BuildSiteKind::Wall(WallMaterial::Mudbrick))
            .expect("architect should cover the Mudbrick wall");
        assert_eq!(resolved.class, ConstructionPosterClass::Architect);

        // A no-tech Bed resolves to the chief (preferred when capable).
        let resolved = pool
            .select_poster_for_kind(3, Some(sid), BuildSiteKind::Bed)
            .expect("chief covers a no-tech Bed");
        assert_eq!(resolved.class, ConstructionPosterClass::Chief);

        // Unknown faction → no poster.
        assert!(pool
            .select_poster_for_kind(99, Some(sid), BuildSiteKind::Bed)
            .is_none());
    }

    // ---- Step 2: era-aware material selector + scarcity classifier ----

    use crate::economy::core_ids;

    fn neo_techs() -> FactionTechs {
        techs_through_era(Era::Neolithic)
    }

    /// Build a view that classifies `rid` at exactly the requested scarcity
    /// tier via `classify_resource` (so the test exercises the real classifier
    /// rather than hand-stamping `ResourceAvailability`).
    fn view_with(
        entries: &[(crate::economy::resource_catalog::ResourceId, Scarcity)],
    ) -> MaterialAvailabilityView {
        let mut v = MaterialAvailabilityView::default();
        for &(rid, sc) in entries {
            let av = match sc {
                Scarcity::Available => classify_resource(8, 8, 0.0, 0.0, 0.0, false, 1),
                Scarcity::Tight => classify_resource(0, 0, 0.0, 0.0, 0.0, true, 1),
                Scarcity::Scarce => classify_resource(0, 0, 99.0, 2.0, 999.0, false, 1),
                Scarcity::Unavailable => classify_resource(0, 0, 0.0, 0.0, 0.0, false, 1),
            };
            assert_eq!(av.scarcity, sc, "view_with mis-classified {:?}", sc);
            v.insert(rid, av);
        }
        v
    }

    #[test]
    fn classify_returns_available_when_stored() {
        let a = classify_resource(5, 5, 0.0, 0.0, 0.0, false, 3);
        assert_eq!(a.scarcity, Scarcity::Available);
    }

    #[test]
    fn classify_tight_when_gatherable() {
        let a = classify_resource(0, 0, 0.0, 0.0, 0.0, true, 3);
        assert_eq!(a.scarcity, Scarcity::Tight);
    }

    #[test]
    fn classify_returns_scarce_when_market_affordable_not_gatherable() {
        // stored 0, not gatherable, market has 10 @ price 2, budget 100 →
        // affordable_qty = min(floor(100/2), 10) = 10 ≥ need(3).
        let a = classify_resource(0, 0, 10.0, 2.0, 100.0, false, 3);
        assert_eq!(a.scarcity, Scarcity::Scarce);
        assert_eq!(a.affordable_qty, 10);
        assert_eq!(a.market_price, 2.0);
    }

    #[test]
    fn classify_returns_unavailable_when_broke_and_no_market() {
        let a = classify_resource(0, 0, 0.0, 0.0, 0.0, false, 3);
        assert_eq!(a.scarcity, Scarcity::Unavailable);
        assert_eq!(a.affordable_qty, 0);
    }

    #[test]
    fn classify_not_available_when_stock0_supply_positive() {
        // Deposited-only rule: agent-held inventory must NOT read as Available.
        let a = classify_resource(0, 9, 0.0, 0.0, 0.0, false, 3);
        assert_ne!(a.scarcity, Scarcity::Available);
        assert_eq!(a.scarcity, Scarcity::Unavailable);
        assert_eq!(a.inventory, 9);
        assert_eq!(a.stored, 0);
    }

    #[test]
    fn select_unconstrained_none_equals_best_wall_material() {
        for era in [
            Era::Paleolithic,
            Era::Mesolithic,
            Era::Neolithic,
            Era::Chalcolithic,
            Era::BronzeAge,
        ] {
            let t = techs_through_era(era);
            assert_eq!(
                select_wall_material(&t, None).mat(),
                Some(best_wall_material(&t)),
                "selector(None) must equal best_wall_material for {:?}",
                era
            );
        }
    }

    #[test]
    fn select_keeps_top_when_available() {
        // Neolithic top rung = Mudbrick (stone 2 + wood 1).
        let t = neo_techs();
        assert_eq!(best_wall_material(&t), WallMaterial::Mudbrick);
        let v = view_with(&[
            (core_ids::stone(), Scarcity::Available),
            (core_ids::wood(), Scarcity::Available),
        ]);
        assert_eq!(
            select_wall_material(&t, Some(&v)),
            WallSelection::Material {
                mat: WallMaterial::Mudbrick,
                source: HaulSource::Storage,
            }
        );
    }

    #[test]
    fn select_keeps_top_with_market_source_when_scarce() {
        let t = neo_techs();
        let v = view_with(&[
            (core_ids::stone(), Scarcity::Scarce),
            (core_ids::wood(), Scarcity::Available),
        ]);
        match select_wall_material(&t, Some(&v)) {
            WallSelection::Material {
                mat: WallMaterial::Mudbrick,
                source: HaulSource::Market { max_unit_price },
            } => {
                assert!(max_unit_price > 0.0, "market source must carry a price");
            }
            other => panic!("expected Mudbrick + Market, got {:?}", other),
        }
    }

    #[test]
    fn select_steps_down_ladder_when_unavailable() {
        // Mudbrick needs stone; mark stone Unavailable but WattleDaub's inputs
        // (wood + grain) available → step down to WattleDaub (not emergency).
        let t = neo_techs();
        let v = view_with(&[
            (core_ids::stone(), Scarcity::Unavailable),
            (core_ids::wood(), Scarcity::Available),
            (core_ids::grain(), Scarcity::Available),
        ]);
        assert_eq!(
            select_wall_material(&t, Some(&v)).mat(),
            Some(WallMaterial::WattleDaub)
        );
    }

    #[test]
    fn select_returns_emergency_when_all_rungs_unavailable() {
        let t = neo_techs();
        // Empty view → every input classifies Unavailable → no rung buildable.
        let v = MaterialAvailabilityView::default();
        assert_eq!(
            select_wall_material(&t, Some(&v)),
            WallSelection::EmergencyShelter
        );
    }
}
