//! Organic settlement AI.
//!
//! This is the planning layer above the existing blueprint / project / job
//! machinery. It keeps persistent per-settlement context (anchors, parcels,
//! traffic heat, soft districts), derives practical pressures, turns those
//! pressures into build intents, and lets `construction::chief_directive_system`
//! execute the selected intent through the normal construction backend.

use ahash::{AHashMap, AHashSet};
use bevy::ecs::system::SystemParam;
use bevy::prelude::*;
use serde::Deserialize;

use crate::game_state::StartSettlementMaturity;
use crate::simulation::building_template::{FootprintShape, Rotation};
use crate::simulation::civic_milestones::{civic_milestone_allows, should_seed_civic, CivicKind};
use crate::simulation::construction::{
    best_wall_material, faction_can_build, find_emergency_bed_tile, recipe_for,
    select_wall_material, BarracksMap, BedMap, Blueprint, BlueprintMap, BuildSiteKind, CampfireMap,
    DamMap, DoorMap, GranaryMap, LoomMap, MarketMap, MonumentMap, RoadCarveQueue, ShrineMap,
    StructureIndex, TableMap, WallMap, WallMaterial, WallSelection, WellMap, WorkbenchMap,
    MAX_BLUEPRINTS_SAFETY_CAP,
};
use crate::simulation::faction::{
    FactionCulture, FactionData, FactionMember, FactionRegistry, FactionTechs, SOLO,
};
use crate::simulation::land::{tile_buildable_by, Plot, PlotIndex, TenureHolder, TileEdge};
use crate::simulation::schedule::SimClock;
use crate::simulation::settlement::{
    Settlement, SettlementId, SettlementPlan, StreetSegment, StreetSpine, StreetTier, TileRect,
    Zone, ZoneKind,
};
use crate::simulation::technology::{
    current_era, Era, TechId, BRIDGE_BUILDING, CITY_STATE_ORG, CROP_CULTIVATION, DAM_BUILDING,
    FLINT_KNAPPING, GRANARY, LONG_DIST_TRADE, MONUMENTAL_BUILDING, PERM_SETTLEMENT,
    PROFESSIONAL_ARMY, SACRED_RITUAL, WELL_DIGGING,
};
use crate::simulation::terraform::PendingFootprints;
use crate::world::chunk::ChunkMap;
use crate::world::seasons::Calendar;
use crate::world::terrain::world_to_tile;
use crate::world::tile::TileKind;

const SURVEY_INTERVAL: u64 = 120;
const PRESSURE_INTERVAL: u64 = 60;
const DESIRE_PATH_INTERVAL: u64 = 900;
const MAX_FRONTIER: usize = 96;
const MAX_PARCELS: usize = 48;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettlementPhase {
    Camp,
    Hamlet,
    Village,
    Chiefdom,
    ProtoUrban,
    Urban,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SettlementAnchorKind {
    Hearth,
    WaterAccess,
    Storehouse,
    Field,
    Shrine,
    Workshop,
    Market,
    Gate,
    HighGround,
    MaterialPatch,
    CivicCore,
}

#[derive(Clone, Debug)]
pub struct SettlementAnchor {
    pub kind: SettlementAnchorKind,
    pub tile: (i32, i32),
    pub weight: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DistrictKind {
    Residential,
    Agricultural,
    Crafting,
    Civic,
    Defense,
    Storage,
    Sacred,
    Market,
}

impl DistrictKind {
    pub fn zone_kind(self) -> ZoneKind {
        match self {
            DistrictKind::Residential => ZoneKind::Residential,
            DistrictKind::Agricultural => ZoneKind::Agricultural,
            DistrictKind::Crafting => ZoneKind::Crafting,
            DistrictKind::Civic => ZoneKind::Civic,
            DistrictKind::Defense => ZoneKind::Defense,
            DistrictKind::Storage => ZoneKind::Storage,
            DistrictKind::Sacred => ZoneKind::Sacred,
            DistrictKind::Market => ZoneKind::Market,
        }
    }

    pub fn from_zone_kind(zk: ZoneKind) -> Self {
        match zk {
            ZoneKind::Residential => DistrictKind::Residential,
            ZoneKind::Agricultural => DistrictKind::Agricultural,
            ZoneKind::Crafting => DistrictKind::Crafting,
            ZoneKind::Civic => DistrictKind::Civic,
            ZoneKind::Defense => DistrictKind::Defense,
            ZoneKind::Storage => DistrictKind::Storage,
            ZoneKind::Sacred => DistrictKind::Sacred,
            ZoneKind::Market => DistrictKind::Market,
        }
    }
}

#[derive(Clone, Debug)]
pub struct DistrictInfluence {
    pub kind: DistrictKind,
    pub centre: (i32, i32),
    pub radius: u8,
    pub weight: f32,
}

#[derive(Clone, Debug)]
pub enum ParcelShape {
    Rect(TileRect),
    Shape {
        shape: FootprintShape,
        rotation: Rotation,
        anchor: (i32, i32),
    },
}

#[derive(Clone, Debug, Default)]
pub struct ParcelSuitability {
    pub residential: f32,
    pub agricultural: f32,
    pub crafting: f32,
    pub civic: f32,
    pub defense: f32,
    pub storage: f32,
    pub sacred: f32,
    pub market: f32,
}

impl ParcelSuitability {
    fn for_district(&self, kind: DistrictKind) -> f32 {
        match kind {
            DistrictKind::Residential => self.residential,
            DistrictKind::Agricultural => self.agricultural,
            DistrictKind::Crafting => self.crafting,
            DistrictKind::Civic => self.civic,
            DistrictKind::Defense => self.defense,
            DistrictKind::Storage => self.storage,
            DistrictKind::Sacred => self.sacred,
            DistrictKind::Market => self.market,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Parcel {
    pub id: u32,
    pub shape: ParcelShape,
    pub frontage_edge: Option<TileEdge>,
    pub access_tile: Option<(i32, i32)>,
    pub holder: TenureHolder,
    pub district_hint: Option<DistrictKind>,
    pub suitability: ParcelSuitability,
}

impl Parcel {
    pub fn centre(&self) -> (i32, i32) {
        match self.shape {
            ParcelShape::Rect(rect) => rect.center(),
            ParcelShape::Shape { anchor, .. } => anchor,
        }
    }

    fn rect(&self) -> TileRect {
        match self.shape {
            ParcelShape::Rect(rect) => rect,
            ParcelShape::Shape { anchor, .. } => TileRect::new(anchor.0, anchor.1, 1, 1),
        }
    }
}

#[derive(Clone, Debug)]
pub struct SettlementBrain {
    pub settlement_id: SettlementId,
    pub owner_faction: u32,
    pub home_tile: (i32, i32),
    pub phase: SettlementPhase,
    pub anchors: Vec<SettlementAnchor>,
    pub districts: Vec<DistrictInfluence>,
    pub road_segments: Vec<StreetSegment>,
    pub road_tiles: AHashSet<(i32, i32)>,
    /// Widened road footprint that mirrors what `road_carve_system` actually
    /// stamps: every centerline tile plus the dominant-axis perpendicular
    /// widening tile. Parcel allocation rejects against this so a planner
    /// reservation can't be clipped by the carver.
    pub road_corridor_tiles: AHashSet<(i32, i32)>,
    /// Phase-keyed civic commons keepout disc around `home_tile`. Civic
    /// builds (well/granary/shrine/market/hearth/bureaucrat) may stamp
    /// inside; residential / crafting / storage (warehouse) / sacred /
    /// defense / composite-house must stay outside.
    pub commons_rect: Option<TileRect>,
    pub parcels: Vec<Parcel>,
    pub traffic_heat: AHashMap<(i32, i32), u8>,
    pub frontier: Vec<(i32, i32)>,
    pub seed: u64,
    pub layout_hash: u64,
    pub last_survey_tick: u64,
    pub last_path_carve_tick: u64,
}

impl SettlementBrain {
    pub fn new(settlement_id: SettlementId, owner_faction: u32, seed: u64) -> Self {
        Self {
            settlement_id,
            owner_faction,
            home_tile: (0, 0),
            phase: SettlementPhase::Camp,
            anchors: Vec::new(),
            districts: Vec::new(),
            road_segments: Vec::new(),
            road_tiles: AHashSet::default(),
            road_corridor_tiles: AHashSet::default(),
            commons_rect: None,
            parcels: Vec::new(),
            traffic_heat: AHashMap::default(),
            frontier: Vec::new(),
            seed,
            layout_hash: seed,
            last_survey_tick: 0,
            last_path_carve_tick: 0,
        }
    }
}

/// Phase-keyed chebyshev radius for the civic commons disc around `home_tile`.
/// Camp has no commons; settled phases reserve a small ring (5×5 / 7×7) for
/// civic-only construction.
pub(crate) fn commons_radius(phase: SettlementPhase) -> i32 {
    match phase {
        SettlementPhase::Camp => 0,
        SettlementPhase::Hamlet | SettlementPhase::Village => 2,
        SettlementPhase::Chiefdom | SettlementPhase::ProtoUrban | SettlementPhase::Urban => 3,
    }
}

/// Build the `commons_rect` keepout disc for a settlement.
pub(crate) fn commons_rect_for(home: (i32, i32), phase: SettlementPhase) -> Option<TileRect> {
    let r = commons_radius(phase);
    if r <= 0 {
        return None;
    }
    Some(TileRect::new(
        home.0 - r,
        home.1 - r,
        (2 * r + 1) as u16,
        (2 * r + 1) as u16,
    ))
}

/// True if `tile` sits inside the commons keepout.
pub(crate) fn tile_inside_commons(commons: Option<TileRect>, tile: (i32, i32)) -> bool {
    commons.map_or(false, |r| r.contains(tile.0, tile.1))
}

/// True if `rect` intersects the commons keepout.
pub(crate) fn rect_intersects_commons(commons: Option<TileRect>, rect: TileRect) -> bool {
    let Some(c) = commons else {
        return false;
    };
    let ax0 = rect.x0;
    let ay0 = rect.y0;
    let ax1 = rect.x0 + rect.w as i32;
    let ay1 = rect.y0 + rect.h as i32;
    let bx0 = c.x0;
    let by0 = c.y0;
    let bx1 = c.x0 + c.w as i32;
    let by1 = c.y0 + c.h as i32;
    ax0 < bx1 && bx0 < ax1 && ay0 < by1 && by0 < ay1
}

/// Districts allowed to stamp inside the civic commons.
pub(crate) fn district_allowed_in_commons(kind: DistrictKind) -> bool {
    matches!(kind, DistrictKind::Civic)
}

/// `OrganicBuildKind`s allowed inside the civic commons (well, granary,
/// shrine, market, hearth/campfire, bureaucrat office, table). Residential,
/// crafting workshops, storage warehouses, defense, sacred monuments, and
/// composite/walled houses are barred.
pub(crate) fn build_kind_allowed_in_commons(build_kind: OrganicBuildKind) -> bool {
    match build_kind {
        OrganicBuildKind::Single(kind) => matches!(
            kind,
            BuildSiteKind::Campfire
                | BuildSiteKind::Granary
                | BuildSiteKind::Shrine
                | BuildSiteKind::Market
                | BuildSiteKind::Well
                | BuildSiteKind::Table
        ),
        OrganicBuildKind::Hut(_)
        | OrganicBuildKind::Longhouse { .. }
        | OrganicBuildKind::PalisadeSegment(_)
        | OrganicBuildKind::CompositeHouse { .. } => false,
    }
}

#[derive(Resource, Default)]
pub struct SettlementBrains(pub AHashMap<SettlementId, SettlementBrain>);

#[derive(Resource, Default)]
pub struct SettlementParcelIndex {
    pub by_tile: AHashMap<(i32, i32), (SettlementId, u32)>,
    pub by_settlement: AHashMap<SettlementId, Vec<u32>>,
    pub by_faction: AHashMap<u32, Vec<(SettlementId, u32)>>,
}

impl SettlementParcelIndex {
    pub fn rebuild(&mut self, brains: &SettlementBrains) {
        self.by_tile.clear();
        self.by_settlement.clear();
        self.by_faction.clear();
        for (sid, brain) in &brains.0 {
            for parcel in &brain.parcels {
                self.by_settlement.entry(*sid).or_default().push(parcel.id);
                self.by_faction
                    .entry(brain.owner_faction)
                    .or_default()
                    .push((*sid, parcel.id));
                let rect = parcel.rect();
                for y in rect.y0..rect.y0 + rect.h as i32 {
                    for x in rect.x0..rect.x0 + rect.w as i32 {
                        self.by_tile.insert((x, y), (*sid, parcel.id));
                    }
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SettlementPressureKind {
    Hearth,
    Shelter,
    Field,
    Storage,
    Craft,
    Ritual,
    Trade,
    Defense,
    Military,
    Monument,
    Governance,
    WaterAccess,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettlementSponsor {
    Chief,
    Household(u32),
    Bureaucracy,
}

#[derive(Clone, Debug)]
pub struct SettlementPressure {
    pub kind: SettlementPressureKind,
    pub urgency: f32,
    pub sponsor: SettlementSponsor,
    pub population_scope: u32,
    pub material_budget: u32,
    pub reason: &'static str,
}

#[derive(Resource, Default)]
pub struct SettlementPressureMap(pub AHashMap<u32, Vec<SettlementPressure>>);

/// Long-axis orientation for a Longhouse footprint. `EastWest` = 5×3
/// (`half_w=2, half_h=1`); `NorthSouth` = 3×5 (`half_w=1, half_h=2`).
/// Picked by the residential evaluator based on which orientation produces
/// the cleanest entrance route — `EastWest` is the legacy default kept for
/// non-evaluator emitters (seed retries, fixtures).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HouseAxis {
    EastWest,
    NorthSouth,
}

impl HouseAxis {
    /// Footprint half-dimensions `(half_w, half_h)` for this axis.
    #[inline]
    pub fn longhouse_halves(self) -> (i32, i32) {
        match self {
            HouseAxis::EastWest => (2, 1),
            HouseAxis::NorthSouth => (1, 2),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OrganicBuildKind {
    Single(BuildSiteKind),
    Hut(WallMaterial),
    Longhouse {
        wall_material: WallMaterial,
        axis: HouseAxis,
    },
    PalisadeSegment(WallMaterial),
    CompositeHouse {
        shape: FootprintShape,
        rotation: Rotation,
        wall_material: WallMaterial,
    },
}

#[derive(Clone, Debug)]
pub struct ConstructionIntent {
    pub template_id: String,
    pub build_kind: OrganicBuildKind,
    pub tile: (i32, i32),
    pub door_dir: Option<TileEdge>,
    pub sponsor: SettlementSponsor,
    pub priority: f32,
    pub reason: &'static str,
    /// Role to stamp on the finished `Campfire`. Only meaningful when
    /// `build_kind` resolves to `BuildSiteKind::Campfire`. `None` for
    /// every non-Hearth intent (workbench, shrine, …).
    pub hearth_role: Option<crate::simulation::construction::HearthRole>,
}

#[derive(Resource, Default)]
pub struct SettlementIntentMap(pub AHashMap<u32, Vec<ConstructionIntent>>);

#[derive(Resource, Default)]
pub struct SelectedSettlementIntents(pub AHashMap<u32, ConstructionIntent>);

#[derive(Clone, Debug, Deserialize)]
pub struct BuildingArchetypeFile {
    pub archetypes: Vec<BuildingArchetype>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct BuildingArchetype {
    pub id: String,
    pub era_range: EraRangeDef,
    #[serde(default)]
    pub culture_tags: Vec<String>,
    #[serde(default)]
    pub biome_tags: Vec<String>,
    #[serde(default)]
    pub footprint_options: Vec<FootprintOptionDef>,
    pub capacity: u8,
    #[serde(default)]
    pub adjacency_rules: Vec<PlacementRuleDef>,
    #[serde(default)]
    pub avoid_rules: Vec<PlacementRuleDef>,
    pub material_policy: MaterialPolicyDef,
    #[serde(default)]
    pub upgrades_to: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct EraRangeDef {
    pub min: EraDef,
    pub max: EraDef,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EraDef {
    Paleolithic,
    Mesolithic,
    Neolithic,
    Chalcolithic,
    BronzeAge,
}

impl EraDef {
    fn era(self) -> Era {
        match self {
            EraDef::Paleolithic => Era::Paleolithic,
            EraDef::Mesolithic => Era::Mesolithic,
            EraDef::Neolithic => Era::Neolithic,
            EraDef::Chalcolithic => Era::Chalcolithic,
            EraDef::BronzeAge => Era::BronzeAge,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FootprintOptionDef {
    Single,
    Hut,
    Longhouse,
    LShapeFarmstead,
    Courtyard,
    PalisadeSegment,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PlacementRuleDef {
    pub target: String,
    pub weight: f32,
    #[serde(default)]
    pub radius: Option<u8>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaterialPolicyDef {
    BestAvailable,
    LightShelter,
    CivicDurable,
    Defensive,
}

#[derive(Resource, Default)]
pub struct BuildingArchetypeCatalog {
    pub by_id: AHashMap<String, BuildingArchetype>,
}

impl BuildingArchetypeCatalog {
    pub fn for_era(&self, era: Era) -> impl Iterator<Item = &BuildingArchetype> {
        self.by_id.values().filter(move |a| {
            let min = a.era_range.min.era() as u8;
            let max = a.era_range.max.era() as u8;
            let e = era as u8;
            e >= min && e <= max
        })
    }
}

pub fn load_building_archetype_catalog() -> BuildingArchetypeCatalog {
    let dir = std::path::Path::new("assets/data/settlements");
    let mut by_id = AHashMap::default();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return BuildingArchetypeCatalog { by_id };
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("ron") {
            continue;
        }
        let body = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("BuildingArchetypeCatalog: cannot read {:?}: {}", path, e));
        let file: BuildingArchetypeFile = ron::from_str(&body).unwrap_or_else(|e| {
            panic!("BuildingArchetypeCatalog: parse error in {:?}: {}", path, e)
        });
        for archetype in file.archetypes {
            by_id.insert(archetype.id.clone(), archetype);
        }
    }
    BuildingArchetypeCatalog { by_id }
}

/// SystemParam bundle of structure-map `Res`es. Driver systems take this
/// directly; internal helpers take `OrganicStructureMaps<'_>` (the borrowed
/// view below) instead, so the seed pipeline (which holds a `FurnitureMaps`
/// `ResMut` bundle) can re-use the same helpers without going through
/// `Res<T>`.
#[derive(SystemParam)]
pub struct OrganicStructureMapsParam<'w> {
    pub bed_map: Res<'w, BedMap>,
    pub wall_map: Res<'w, WallMap>,
    pub campfire_map: Res<'w, CampfireMap>,
    pub door_map: Res<'w, DoorMap>,
    pub workbench_map: Res<'w, WorkbenchMap>,
    pub loom_map: Res<'w, LoomMap>,
    pub table_map: Res<'w, TableMap>,
    pub granary_map: Res<'w, GranaryMap>,
    pub shrine_map: Res<'w, ShrineMap>,
    pub market_map: Res<'w, MarketMap>,
    pub barracks_map: Res<'w, BarracksMap>,
    pub monument_map: Res<'w, MonumentMap>,
    pub well_map: Res<'w, WellMap>,
    pub structure_index: Res<'w, StructureIndex>,
}

impl<'w> OrganicStructureMapsParam<'w> {
    /// Borrow the inner map references as the lightweight
    /// `OrganicStructureMaps<'_>` view that every internal helper consumes.
    pub fn view(&self) -> OrganicStructureMaps<'_> {
        OrganicStructureMaps {
            bed_map: &*self.bed_map,
            wall_map: &*self.wall_map,
            campfire_map: &*self.campfire_map,
            door_map: &*self.door_map,
            workbench_map: &*self.workbench_map,
            loom_map: &*self.loom_map,
            table_map: &*self.table_map,
            granary_map: &*self.granary_map,
            shrine_map: &*self.shrine_map,
            market_map: &*self.market_map,
            barracks_map: &*self.barracks_map,
            monument_map: &*self.monument_map,
            well_map: &*self.well_map,
            structure_index: &*self.structure_index,
        }
    }
}

/// Lightweight borrowed view of the structure-map set. Helpers
/// (`append_pressures_for_faction`, `pressure_to_intent`, parcel
/// builders, etc.) accept this so the same code path serves runtime
/// (`OrganicStructureMapsParam::view`) and the seed pipeline's
/// `FurnitureMaps::organic_view` without depending on `Res<T>`.
pub struct OrganicStructureMaps<'a> {
    pub bed_map: &'a BedMap,
    pub wall_map: &'a WallMap,
    pub campfire_map: &'a CampfireMap,
    pub door_map: &'a DoorMap,
    pub workbench_map: &'a WorkbenchMap,
    pub loom_map: &'a LoomMap,
    pub table_map: &'a TableMap,
    pub granary_map: &'a GranaryMap,
    pub shrine_map: &'a ShrineMap,
    pub market_map: &'a MarketMap,
    pub barracks_map: &'a BarracksMap,
    pub monument_map: &'a MonumentMap,
    pub well_map: &'a WellMap,
    pub structure_index: &'a StructureIndex,
}

/// Send-able structure-map view captured on the main thread before a
/// settlement survey runs. The survey only needs tile occupancy/anchor keys,
/// never the live entities behind those maps.
#[derive(Clone, Default)]
pub struct SurveyStructureSnapshot {
    pub beds: AHashSet<(i32, i32)>,
    pub walls: AHashSet<(i32, i32)>,
    pub campfires: AHashSet<(i32, i32)>,
    pub doors: AHashSet<(i32, i32)>,
    pub workbenches: AHashSet<(i32, i32)>,
    pub looms: AHashSet<(i32, i32)>,
    pub tables: AHashSet<(i32, i32)>,
    pub granaries: AHashSet<(i32, i32)>,
    pub shrines: AHashSet<(i32, i32)>,
    pub markets: AHashSet<(i32, i32)>,
    pub barracks: AHashSet<(i32, i32)>,
    pub monuments: AHashSet<(i32, i32)>,
    pub wells: AHashSet<(i32, i32)>,
    pub structures: AHashSet<(i32, i32)>,
}

impl SurveyStructureSnapshot {
    /// Capture all tiles within the settlement survey window. This keeps the
    /// async task input small while preserving every map lookup the survey
    /// actually performs.
    pub fn capture(maps: &OrganicStructureMaps, centre: (i32, i32)) -> Self {
        let half = crate::simulation::survey_task::SURVEY_WINDOW;
        let in_window = |(x, y): &(i32, i32)| -> bool {
            (x - centre.0).abs() <= half && (y - centre.1).abs() <= half
        };
        fn pick<V>(
            m: &AHashMap<(i32, i32), V>,
            in_window: impl Fn(&(i32, i32)) -> bool,
        ) -> AHashSet<(i32, i32)> {
            m.keys().filter(|t| in_window(t)).copied().collect()
        }
        Self {
            beds: pick(&maps.bed_map.0, in_window),
            walls: pick(&maps.wall_map.0, in_window),
            campfires: pick(&maps.campfire_map.0, in_window),
            doors: maps
                .door_map
                .0
                .keys()
                .filter(|t| in_window(t))
                .copied()
                .collect(),
            workbenches: pick(&maps.workbench_map.0, in_window),
            looms: pick(&maps.loom_map.0, in_window),
            tables: pick(&maps.table_map.0, in_window),
            granaries: pick(&maps.granary_map.0, in_window),
            shrines: pick(&maps.shrine_map.0, in_window),
            markets: pick(&maps.market_map.0, in_window),
            barracks: pick(&maps.barracks_map.0, in_window),
            monuments: pick(&maps.monument_map.0, in_window),
            wells: pick(&maps.well_map.0, in_window),
            structures: maps
                .structure_index
                .0
                .keys()
                .filter(|t| in_window(t))
                .copied()
                .collect(),
        }
    }
}

trait SurveyFactionView {
    fn home_tile(&self) -> (i32, i32);
    fn member_count(&self) -> u32;
    fn techs(&self) -> FactionTechs;
    fn buildable_techs(&self) -> FactionTechs;
    fn culture(&self) -> &FactionCulture;
    fn seed_total(&self) -> u32;

    #[inline]
    fn community_has(&self, tech: TechId) -> bool {
        self.buildable_techs().has(tech)
    }
}

impl SurveyFactionView for FactionData {
    fn home_tile(&self) -> (i32, i32) {
        self.home_tile
    }

    fn member_count(&self) -> u32 {
        self.member_count
    }

    fn techs(&self) -> FactionTechs {
        self.techs
    }

    fn buildable_techs(&self) -> FactionTechs {
        self.buildable_techs
    }

    fn culture(&self) -> &FactionCulture {
        &self.culture
    }

    fn seed_total(&self) -> u32 {
        self.storage.seed_total()
    }
}

#[derive(Clone)]
pub struct SurveyFactionSnapshot {
    pub home_tile: (i32, i32),
    pub member_count: u32,
    pub techs: FactionTechs,
    pub buildable_techs: FactionTechs,
    pub culture: FactionCulture,
    pub seed_total: u32,
}

impl SurveyFactionSnapshot {
    pub fn from_faction(faction: &FactionData) -> Self {
        Self {
            home_tile: faction.home_tile,
            member_count: faction.member_count,
            techs: faction.techs,
            buildable_techs: faction.buildable_techs,
            culture: faction.culture.clone(),
            seed_total: faction.storage.seed_total(),
        }
    }
}

impl SurveyFactionView for SurveyFactionSnapshot {
    fn home_tile(&self) -> (i32, i32) {
        self.home_tile
    }

    fn member_count(&self) -> u32 {
        self.member_count
    }

    fn techs(&self) -> FactionTechs {
        self.techs
    }

    fn buildable_techs(&self) -> FactionTechs {
        self.buildable_techs
    }

    fn culture(&self) -> &FactionCulture {
        &self.culture
    }

    fn seed_total(&self) -> u32 {
        self.seed_total
    }
}

pub struct SettlementSurveyInput {
    pub settlement: Settlement,
    pub faction: SurveyFactionSnapshot,
    pub tick: u64,
    pub prior_brain: Option<SettlementBrain>,
    pub chunk_map: ChunkMap,
    pub maps: SurveyStructureSnapshot,
    pub member_offsets: Vec<(i32, i32)>,
    pub snapshot_chunks: usize,
    /// Sticky-farm input: rects of every Agricultural `Plot` currently
    /// owned by this settlement. `build_ag_belt` pre-accepts these so the
    /// belt cannot relocate for scoring or demand-shift reasons — only a
    /// hard non-Ag-district overlap can release tiles, and only through
    /// the carve's per-tile retirement path.
    pub committed_ag_rects: Vec<TileRect>,
}

pub struct SettlementSurveyDiff {
    pub settlement_id: SettlementId,
    pub owner_faction: u32,
    pub faction_home_tile: (i32, i32),
    pub peak_population: u32,
    pub tick: u64,
    pub brain: SettlementBrain,
    pub road_pushes: Vec<(u32, (i32, i32), (i32, i32))>,
    pub snapshot_chunks: usize,
}

pub fn compute_settlement_survey(input: SettlementSurveyInput) -> SettlementSurveyDiff {
    compute_settlement_survey_core(
        &input.settlement,
        &input.faction,
        input.tick,
        input.prior_brain,
        &input.chunk_map,
        &input.maps,
        &input.member_offsets,
        input.snapshot_chunks,
        &input.committed_ag_rects,
    )
}

#[allow(clippy::too_many_arguments)]
fn compute_settlement_survey_core<F: SurveyFactionView>(
    settlement: &Settlement,
    faction: &F,
    tick: u64,
    prior_brain: Option<SettlementBrain>,
    chunk_map: &ChunkMap,
    maps: &SurveyStructureSnapshot,
    member_offsets: &[(i32, i32)],
    snapshot_chunks: usize,
    committed_ag_rects: &[TileRect],
) -> SettlementSurveyDiff {
    let seed = organic_seed(settlement, faction);
    let mut brain = prior_brain
        .unwrap_or_else(|| SettlementBrain::new(settlement.id, settlement.owner_faction, seed));

    brain.owner_faction = settlement.owner_faction;
    brain.home_tile = faction.home_tile();
    brain.seed = seed;
    brain.phase = phase_for(faction, settlement.peak_population);
    brain.commons_rect = commons_rect_for(faction.home_tile(), brain.phase);
    brain.last_survey_tick = tick;
    decay_traffic(&mut brain.traffic_heat);
    accumulate_traffic_offsets(&mut brain, faction.home_tile(), member_offsets);
    brain.anchors = collect_anchors(faction, settlement, chunk_map, maps);
    brain.districts = build_districts(faction, settlement, &brain);
    brain.road_segments = build_road_network(faction, &brain, chunk_map, member_offsets);
    brain.road_tiles = road_tiles_for_segments(&brain.road_segments);
    // Route the widened corridor around standing structures captured in the
    // survey snapshot so the planner never reserves (and the carver never
    // tries to paint) a road tile under a finished building.
    brain.road_corridor_tiles =
        road_corridor_tiles_for_segments_with(&brain.road_segments, |t| {
            maps.structures.contains(&t)
        });
    brain.frontier = build_frontier(faction, &brain, chunk_map, maps);
    brain.parcels = build_parcels(faction, settlement, &brain, chunk_map, maps, committed_ag_rects);
    brain.layout_hash = layout_hash(faction, &brain);

    let road_pushes = desire_path_push(
        &mut brain,
        faction.home_tile(),
        settlement.owner_faction,
        tick,
        chunk_map,
    )
    .into_iter()
    .collect();

    SettlementSurveyDiff {
        settlement_id: settlement.id,
        owner_faction: settlement.owner_faction,
        faction_home_tile: faction.home_tile(),
        peak_population: settlement.peak_population,
        tick,
        brain,
        road_pushes,
        snapshot_chunks,
    }
}

/// Shared synchronous survey apply body. The async scheduler uses
/// `compute_settlement_survey`; this path is retained for
/// `kickoff_initial_survey_system` at `OnEnter(GameState::Playing)` so
/// `SettlementBrain` exists *before* `seed_starting_buildings_system` picks
/// house anchors. Same effect either way — fold a fresh brain (or update
/// the existing one) into `SettlementBrains`, recompute road segments /
/// parcels / frontier, and enqueue any newly-required desire-path road
/// extensions.
///
/// Caller is expected to have already filtered out SOLO / non-settled
/// factions.
#[allow(clippy::too_many_arguments)]
pub fn survey_one_settlement(
    settlement: &Settlement,
    faction: &FactionData,
    tick: u64,
    brains: &mut SettlementBrains,
    road_queue: &mut RoadCarveQueue,
    chunk_map: &ChunkMap,
    maps: &OrganicStructureMaps,
    member_q: &Query<(&FactionMember, &Transform)>,
    committed_ag_rects: &[TileRect],
) {
    let member_offsets =
        collect_member_offsets(faction.home_tile, settlement.owner_faction, member_q);
    let structure_snapshot = SurveyStructureSnapshot::capture(maps, faction.home_tile);
    let diff = compute_settlement_survey_core(
        settlement,
        faction,
        tick,
        brains.0.get(&settlement.id).cloned(),
        chunk_map,
        &structure_snapshot,
        &member_offsets,
        chunk_map.0.len(),
        committed_ag_rects,
    );
    for road_push in &diff.road_pushes {
        road_queue.0.push(*road_push);
    }
    brains.0.insert(diff.settlement_id, diff.brain);
}

/// Post-seed re-survey run once at `OnEnter(GameState::Playing)` after
/// `seed_starting_buildings_system`. Recomputes each `SettlementBrain`
/// against the *actual built structures* (the pre-seed kickoff brain has
/// no buildings, so `build_ag_belt` keyed off an empty footprint). Without
/// this re-pass the first post-build runtime survey would shift the
/// Agricultural belt out from under any plot we'd just carved, orphaning
/// the seeded Cropland patch — see `plans/spawn-farm-seeding.md`.
///
/// Shares `survey_one_settlement`'s synchronous core with the kickoff pass.
/// Sandbox-bypassed for parity with `seed_starting_buildings_system`.
#[allow(clippy::too_many_arguments)]
pub fn resurvey_after_seeding_system(
    mut brains: ResMut<SettlementBrains>,
    mut parcel_index: ResMut<SettlementParcelIndex>,
    mut road_queue: ResMut<RoadCarveQueue>,
    settlements: Query<&Settlement>,
    registry: Res<FactionRegistry>,
    chunk_map: Res<ChunkMap>,
    maps: OrganicStructureMapsParam,
    member_q: Query<(&FactionMember, &Transform)>,
) {
    let maps = maps.view();
    for settlement in settlements.iter() {
        let Some(faction) = registry.factions.get(&settlement.owner_faction) else {
            continue;
        };
        if settlement.owner_faction == SOLO || !faction.caps.settlement.is_full_settlement() {
            continue;
        }
        // OnEnter call sites run before `carve_plots_system`, so no
        // committed Ag plots exist yet — pass an empty slice. Runtime
        // surveys snapshot real committed rects via `schedule_survey_tasks_system`.
        survey_one_settlement(
            settlement,
            faction,
            0,
            &mut brains,
            &mut road_queue,
            &chunk_map,
            &maps,
            &member_q,
            &[],
        );
    }
    parcel_index.rebuild(&brains);
}

/// Initial-survey pass run once at `OnEnter(GameState::Playing)` so the
/// `seed_starting_buildings_system` can read `SettlementBrain.parcels` (and
/// their `frontage_edge`) when choosing seed-time house anchors. Without
/// this, the very first runtime survey wouldn't fire until tick 120 —
/// well after the seed pass has stamped walled houses on legacy
/// zone-driven anchors that don't sit on road frontage.
///
/// Sandbox-bypassed for parity with `seed_starting_buildings_system`.
#[allow(clippy::too_many_arguments)]
pub fn kickoff_initial_survey_system(
    mut brains: ResMut<SettlementBrains>,
    mut parcel_index: ResMut<SettlementParcelIndex>,
    mut road_queue: ResMut<RoadCarveQueue>,
    settlements: Query<&Settlement>,
    registry: Res<FactionRegistry>,
    chunk_map: Res<ChunkMap>,
    maps: OrganicStructureMapsParam,
    member_q: Query<(&FactionMember, &Transform)>,
) {
    let maps = maps.view();
    for settlement in settlements.iter() {
        let Some(faction) = registry.factions.get(&settlement.owner_faction) else {
            continue;
        };
        if settlement.owner_faction == SOLO || !faction.caps.settlement.is_full_settlement() {
            continue;
        }
        // OnEnter call sites run before `carve_plots_system`, so no
        // committed Ag plots exist yet — pass an empty slice. Runtime
        // surveys snapshot real committed rects via `schedule_survey_tasks_system`.
        survey_one_settlement(
            settlement,
            faction,
            0,
            &mut brains,
            &mut road_queue,
            &chunk_map,
            &maps,
            &member_q,
            &[],
        );
    }
    parcel_index.rebuild(&brains);
}

pub fn settlement_pressure_system(
    clock: Res<SimClock>,
    registry: Res<FactionRegistry>,
    settlements: Query<&Settlement>,
    chunk_map: Res<ChunkMap>,
    maps: OrganicStructureMapsParam,
    bp_map: Res<BlueprintMap>,
    bp_query: Query<&Blueprint>,
    mut pressures: ResMut<SettlementPressureMap>,
) {
    if clock.tick % PRESSURE_INTERVAL != 0 {
        return;
    }
    pressures.0.clear();

    let maps = maps.view();
    let pending = pending_kind_counts(&bp_map, &bp_query);
    for settlement in settlements.iter() {
        let faction_id = settlement.owner_faction;
        let Some(faction) = registry.factions.get(&faction_id) else {
            continue;
        };
        if faction_id == SOLO || faction.member_count == 0 || faction.caps.posting.is_disabled() {
            continue;
        }
        let mut out = Vec::new();
        append_pressures_for_faction(
            faction_id,
            faction,
            settlement,
            &chunk_map,
            &maps,
            pending.get(&faction_id),
            CivicGate::Runtime,
            &mut out,
        );
        if !out.is_empty() {
            out.sort_by(|a, b| {
                b.urgency
                    .partial_cmp(&a.urgency)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            pressures.0.insert(faction_id, out);
        }
    }
}

pub fn settlement_morphology_system(
    clock: Res<SimClock>,
    registry: Res<FactionRegistry>,
    settlements: Query<&Settlement>,
    brains: Res<SettlementBrains>,
    pressures: Res<SettlementPressureMap>,
    mut intents: ResMut<SettlementIntentMap>,
    chunk_map: Res<ChunkMap>,
    maps: OrganicStructureMapsParam,
    bp_map: Res<BlueprintMap>,
    doormat: Res<crate::simulation::doormat::DoormatReservations>,
    archetypes: Res<BuildingArchetypeCatalog>,
) {
    if clock.tick % PRESSURE_INTERVAL != 0 {
        return;
    }
    intents.0.clear();

    let maps = maps.view();
    for settlement in settlements.iter() {
        let faction_id = settlement.owner_faction;
        let Some(faction) = registry.factions.get(&faction_id) else {
            continue;
        };
        let Some(faction_pressures) = pressures.0.get(&faction_id) else {
            continue;
        };
        let Some(brain) = brains.0.get(&settlement.id) else {
            continue;
        };
        let mut chosen_tiles = AHashSet::default();
        let mut faction_intents = Vec::new();
        // One per-faction `RoadField` per planning tick; every residential
        // candidate evaluation downstream is then a hash lookup + bounded
        // local A*. Cost scales with road tile count (not city radius).
        let road_field = crate::simulation::placement_reachability::road_field_from_home(
            &chunk_map,
            &brain.road_tiles,
            faction.home_tile,
        );
        for pressure in faction_pressures {
            if let Some(intent) = pressure_to_intent(
                faction,
                brain,
                pressure,
                &chunk_map,
                &maps,
                &bp_map,
                &doormat,
                &archetypes,
                &mut chosen_tiles,
                CivicGate::Runtime,
                &road_field,
            ) {
                faction_intents.push(intent);
            }
        }
        if !faction_intents.is_empty() {
            faction_intents.sort_by(|a, b| {
                b.priority
                    .partial_cmp(&a.priority)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            intents.0.insert(faction_id, faction_intents);
        }
    }
}

pub fn settlement_project_selection_system(
    clock: Res<SimClock>,
    registry: Res<FactionRegistry>,
    intents: Res<SettlementIntentMap>,
    mut selected: ResMut<SelectedSettlementIntents>,
    bp_map: Res<BlueprintMap>,
    bp_query: Query<&Blueprint>,
    pending_footprints: Res<PendingFootprints>,
    plot_index: Res<PlotIndex>,
    plot_q: Query<&Plot>,
    settlement_map: Res<crate::simulation::settlement::SettlementMap>,
    brains: Res<SettlementBrains>,
    road_queue: Res<RoadCarveQueue>,
    structure_index: Res<crate::simulation::construction::StructureIndex>,
    chunk_map: Res<ChunkMap>,
) {
    if clock.tick % PRESSURE_INTERVAL != 0 {
        return;
    }
    selected.0.clear();

    let bp_count = blueprint_counts_by_faction(&bp_map, &bp_query, &pending_footprints);
    for (&faction_id, faction) in registry.factions.iter() {
        let Some(candidates) = intents.0.get(&faction_id) else {
            continue;
        };
        if faction_id == SOLO || faction.member_count == 0 || faction.caps.posting.is_disabled() {
            continue;
        }
        let count = bp_count.get(&faction_id).copied().unwrap_or(0);
        let concurrent_cap =
            ((faction.member_count as usize / 6).max(2)).min(MAX_BLUEPRINTS_SAFETY_CAP - 1);
        if count >= concurrent_cap {
            continue;
        }
        // Resolve this faction's settlement brain once for the reserved-road
        // filter; absent brain (early survey) skips the road check entirely.
        let brain = settlement_map
            .first_for_faction(faction_id)
            .and_then(|sid| brains.0.get(&sid));
        // Build the queued-road reservation once per faction (not per candidate).
        let queued = queued_road_reservation_for_faction(&road_queue, &structure_index, faction_id);
        let best = candidates
            .iter()
            .filter(|intent| {
                tile_buildable_by(&plot_index, &plot_q, intent.tile, faction_id, None)
                    && intent_tech_allowed(intent.build_kind, faction)
                    && brain.map_or(true, |b| {
                        !intent_touches_reserved_road(
                            intent.build_kind,
                            intent.tile,
                            b,
                            &queued,
                            &chunk_map,
                        )
                    })
            })
            .max_by(|a, b| {
                let ascore = a.priority - material_scarcity_penalty(a.build_kind, faction);
                let bscore = b.priority - material_scarcity_penalty(b.build_kind, faction);
                ascore
                    .partial_cmp(&bscore)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .cloned();
        if let Some(intent) = best {
            selected.0.insert(faction_id, intent);
        }
    }
}

pub fn compat_plan_from_brain(
    faction_id: u32,
    faction: &FactionData,
    tick: u64,
    brain: &SettlementBrain,
) -> SettlementPlan {
    // Emit one Zone per parcel — preserves per-parcel granularity so
    // downstream consumers (chief_directive_system, generate_candidates)
    // produce one candidate per parcel instead of cramming into a
    // single union rect.
    let fallback = crate::simulation::settlement::build_settlement_plan(faction_id, faction, tick);
    let mut zones: Vec<Zone> = Vec::new();
    let mut kinds_with_parcels: AHashSet<ZoneKind> = AHashSet::default();

    for parcel in &brain.parcels {
        let Some(kind) = parcel.district_hint.map(DistrictKind::zone_kind) else {
            continue;
        };
        kinds_with_parcels.insert(kind);
        zones.push(Zone {
            kind,
            rect: parcel.rect(),
            priority: zone_priority(kind, faction),
            capacity: zone_capacity(kind, faction.member_count),
            filled: 0,
        });
    }

    // Districts only fall back as a broad zone when no parcel of that
    // kind exists yet (e.g. early survey before parcels are carved).
    // Agricultural is EXCLUDED: fields come solely from `build_ag_belt`
    // parcels (or frontier-driven camp parcels). A broad district zone
    // here is centred on the home-biased `best_fertile_tile`, which is
    // exactly the "farms carved all over the base" regression.
    for district in &brain.districts {
        let kind = district.kind.zone_kind();
        if kind == ZoneKind::Agricultural {
            continue;
        }
        if kinds_with_parcels.contains(&kind) {
            continue;
        }
        let r = district.radius as i32;
        let rect = TileRect::new(
            district.centre.0 - r,
            district.centre.1 - r,
            (r * 2 + 1) as u16,
            (r * 2 + 1) as u16,
        );
        zones.push(Zone {
            kind,
            rect,
            priority: zone_priority(kind, faction),
            capacity: zone_capacity(kind, faction.member_count),
            filled: 0,
        });
    }

    // The land-tenure layer still carves plots from SettlementPlan zones.
    // Preserve a single legacy fallback zone only when the organic survey
    // has produced neither parcels nor a district for that kind yet.
    // Agricultural is deliberately absent here too — the legacy
    // `build_settlement_plan` ag zone is a home-centred megablock; carving
    // it scattered farms across the whole settlement. No belt parcel ⇒ no
    // Agricultural zone this survey (the belt retries as the layout evolves).
    let kinds_emitted: AHashSet<ZoneKind> = zones.iter().map(|z| z.kind).collect();
    for legacy in fallback.zones.iter().filter(|z| {
        matches!(
            z.kind,
            ZoneKind::Residential | ZoneKind::Crafting | ZoneKind::Storage
        )
    }) {
        if !kinds_emitted.contains(&legacy.kind) {
            zones.push(legacy.clone());
        }
    }

    zones.sort_by_key(|z| z.kind.label());

    if !zones.iter().any(|z| z.kind == ZoneKind::Civic) {
        let home = faction.home_tile;
        zones.push(Zone {
            kind: ZoneKind::Civic,
            rect: TileRect::new(home.0 - 2, home.1 - 2, 5, 5),
            priority: 180,
            capacity: 1,
            filled: 0,
        });
    }

    SettlementPlan {
        zones,
        spine: {
            let organic = organic_street_spine(faction, brain);
            if organic.segments().is_empty() {
                fallback.spine
            } else {
                organic
            }
        },
        planned_at_tick: tick,
        culture_hash: brain.layout_hash ^ ((faction_id as u64) << 32),
    }
}

fn organic_seed<F: SurveyFactionView>(settlement: &Settlement, faction: &F) -> u64 {
    (settlement.id.0 as u64)
        ^ ((faction.culture().seed as u64) << 16)
        ^ ((faction.home_tile().0 as u32 as u64) << 32)
        ^ ((faction.home_tile().1 as u32 as u64) << 1)
}

fn phase_for<F: SurveyFactionView>(faction: &F, peak_population: u32) -> SettlementPhase {
    let techs = faction.techs();
    let era = current_era(&techs);
    if !faction.community_has(PERM_SETTLEMENT) {
        SettlementPhase::Camp
    } else if peak_population < 12 {
        SettlementPhase::Hamlet
    } else if peak_population < 40 {
        SettlementPhase::Village
    } else if (era as u8) < (Era::BronzeAge as u8) {
        SettlementPhase::Chiefdom
    } else if peak_population < 90 {
        SettlementPhase::ProtoUrban
    } else {
        SettlementPhase::Urban
    }
}

fn survey_radius(phase: SettlementPhase) -> i32 {
    match phase {
        SettlementPhase::Camp => 14,
        SettlementPhase::Hamlet => 18,
        SettlementPhase::Village => 24,
        SettlementPhase::Chiefdom => 30,
        SettlementPhase::ProtoUrban => 36,
        SettlementPhase::Urban => 42,
    }
}

fn decay_traffic(traffic: &mut AHashMap<(i32, i32), u8>) {
    traffic.retain(|_, heat| {
        *heat = ((*heat as f32) * 0.82).round() as u8;
        *heat > 2
    });
}

/// Collect `(dx, dy)` offsets from `home` for every faction member.
/// Used by `primary_axis` for PCA-lite cluster axis derivation.
fn collect_member_offsets(
    home: (i32, i32),
    faction_id: u32,
    member_q: &Query<(&FactionMember, &Transform)>,
) -> Vec<(i32, i32)> {
    let mut out = Vec::new();
    for (member, transform) in member_q.iter() {
        if member.faction_id != faction_id {
            continue;
        }
        let tile = world_to_tile(transform.translation.truncate());
        out.push((tile.0 - home.0, tile.1 - home.1));
    }
    out
}

fn accumulate_traffic_offsets(
    brain: &mut SettlementBrain,
    home: (i32, i32),
    member_offsets: &[(i32, i32)],
) {
    let radius = survey_radius(brain.phase) + 10;
    for &(dx, dy) in member_offsets {
        let tile = (home.0 + dx, home.1 + dy);
        if cheb(tile, home) > radius {
            continue;
        }
        let heat = brain.traffic_heat.entry(tile).or_insert(0);
        *heat = heat.saturating_add(18);
    }
}

fn collect_anchors<F: SurveyFactionView>(
    faction: &F,
    settlement: &Settlement,
    chunk_map: &ChunkMap,
    maps: &SurveyStructureSnapshot,
) -> Vec<SettlementAnchor> {
    let home = faction.home_tile();
    let radius = survey_radius(phase_for(faction, settlement.peak_population));
    let mut anchors = vec![SettlementAnchor {
        kind: SettlementAnchorKind::CivicCore,
        tile: home,
        weight: 1.0,
    }];

    add_set_anchors(
        &mut anchors,
        &maps.campfires,
        home,
        32,
        SettlementAnchorKind::Hearth,
        1.0,
    );
    add_set_anchors(
        &mut anchors,
        &maps.granaries,
        home,
        36,
        SettlementAnchorKind::Storehouse,
        0.9,
    );
    add_set_anchors(
        &mut anchors,
        &maps.shrines,
        home,
        36,
        SettlementAnchorKind::Shrine,
        0.75,
    );
    add_set_anchors(
        &mut anchors,
        &maps.workbenches,
        home,
        36,
        SettlementAnchorKind::Workshop,
        0.7,
    );
    add_set_anchors(
        &mut anchors,
        &maps.looms,
        home,
        36,
        SettlementAnchorKind::Workshop,
        0.65,
    );
    add_set_anchors(
        &mut anchors,
        &maps.markets,
        home,
        42,
        SettlementAnchorKind::Market,
        0.8,
    );
    // Built wells are first-class water anchors — orient road / parcel
    // planning around them so the village's water source lands on the
    // street network.
    add_set_anchors(
        &mut anchors,
        &maps.wells,
        home,
        42,
        SettlementAnchorKind::WaterAccess,
        0.9,
    );
    add_door_gate_anchors(&mut anchors, &maps.doors, home, 42);

    if let Some((tile, fresh)) = nearest_water_access(chunk_map, home, radius + 10) {
        anchors.push(SettlementAnchor {
            kind: SettlementAnchorKind::WaterAccess,
            tile,
            weight: if fresh { 1.0 } else { 0.65 },
        });
    }
    if let Some(tile) = best_fertile_tile(chunk_map, home, radius) {
        anchors.push(SettlementAnchor {
            kind: SettlementAnchorKind::Field,
            tile,
            weight: 0.85,
        });
    }
    if let Some(tile) = best_high_ground(chunk_map, home, radius) {
        anchors.push(SettlementAnchor {
            kind: SettlementAnchorKind::HighGround,
            tile,
            weight: 0.55,
        });
    }
    if let Some(tile) = nearest_material_patch(chunk_map, home, radius) {
        anchors.push(SettlementAnchor {
            kind: SettlementAnchorKind::MaterialPatch,
            tile,
            weight: 0.55,
        });
    }

    anchors
}

fn add_set_anchors(
    anchors: &mut Vec<SettlementAnchor>,
    tiles: &AHashSet<(i32, i32)>,
    home: (i32, i32),
    radius: i32,
    kind: SettlementAnchorKind,
    weight: f32,
) {
    for &tile in tiles {
        if cheb(tile, home) <= radius {
            anchors.push(SettlementAnchor { kind, tile, weight });
        }
    }
}

fn add_door_gate_anchors(
    anchors: &mut Vec<SettlementAnchor>,
    tiles: &AHashSet<(i32, i32)>,
    home: (i32, i32),
    radius: i32,
) {
    for &tile in tiles {
        if cheb(tile, home) <= radius && cheb(tile, home) >= 8 {
            anchors.push(SettlementAnchor {
                kind: SettlementAnchorKind::Gate,
                tile,
                weight: 0.55,
            });
        }
    }
}

fn build_districts<F: SurveyFactionView>(
    faction: &F,
    settlement: &Settlement,
    brain: &SettlementBrain,
) -> Vec<DistrictInfluence> {
    let home = faction.home_tile();
    let phase = brain.phase;
    let mut districts = Vec::new();
    districts.push(DistrictInfluence {
        kind: DistrictKind::Civic,
        centre: home,
        radius: 5,
        weight: 1.0,
    });

    for anchor in &brain.anchors {
        match anchor.kind {
            SettlementAnchorKind::Hearth => {
                districts.push(DistrictInfluence {
                    kind: DistrictKind::Residential,
                    centre: anchor.tile,
                    radius: if matches!(phase, SettlementPhase::Camp) {
                        7
                    } else {
                        9
                    },
                    weight: 0.9 * anchor.weight,
                });
            }
            SettlementAnchorKind::Field => {
                districts.push(DistrictInfluence {
                    kind: DistrictKind::Agricultural,
                    centre: anchor.tile,
                    radius: 10,
                    weight: 0.9,
                });
            }
            SettlementAnchorKind::WaterAccess => {
                districts.push(DistrictInfluence {
                    kind: DistrictKind::Residential,
                    centre: midpoint(home, anchor.tile),
                    radius: 8,
                    weight: 0.45,
                });
            }
            SettlementAnchorKind::Storehouse => {
                districts.push(DistrictInfluence {
                    kind: DistrictKind::Storage,
                    centre: anchor.tile,
                    radius: 7,
                    weight: 0.8,
                });
            }
            SettlementAnchorKind::Workshop | SettlementAnchorKind::MaterialPatch => {
                districts.push(DistrictInfluence {
                    kind: DistrictKind::Crafting,
                    centre: anchor.tile,
                    radius: 7,
                    weight: 0.7,
                });
            }
            SettlementAnchorKind::Shrine | SettlementAnchorKind::HighGround => {
                districts.push(DistrictInfluence {
                    kind: DistrictKind::Sacred,
                    centre: anchor.tile,
                    radius: 6,
                    weight: 0.6 + (faction.culture().ceremonial as f32 / 255.0) * 0.35,
                });
            }
            SettlementAnchorKind::Market | SettlementAnchorKind::Gate => {
                districts.push(DistrictInfluence {
                    kind: DistrictKind::Market,
                    centre: anchor.tile,
                    radius: 7,
                    weight: 0.6 + (faction.culture().mercantile as f32 / 255.0) * 0.35,
                });
            }
            SettlementAnchorKind::CivicCore => {}
        }
    }

    if matches!(
        phase,
        SettlementPhase::Chiefdom | SettlementPhase::ProtoUrban | SettlementPhase::Urban
    ) {
        districts.push(DistrictInfluence {
            kind: DistrictKind::Defense,
            centre: home,
            radius: (survey_radius(phase) / 2).clamp(8, 18) as u8,
            weight: 0.45 + (faction.culture().defensive as f32 / 255.0) * 0.55,
        });
    }

    if settlement.peak_population >= 20 {
        districts.push(DistrictInfluence {
            kind: DistrictKind::Storage,
            centre: (home.0 + 3, home.1 - 3),
            radius: 7,
            weight: 0.55,
        });
    }

    districts
}

/// Primary axis for street layout. Derived per settlement from river
/// orientation, dominant external anchors, or member-cluster geometry —
/// never hard-coded to a cardinal default. Diagonals are supported so
/// settlements following a NE-SW river or trade route can run their
/// spine along it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpokeAxis {
    EW,
    NS,
    NeSw,
    NwSe,
}

impl SpokeAxis {
    /// Unit-step along the axis. Diagonals step by (±1, ±1) per Bresenham
    /// tile so segment endpoints reach `radius` chebyshev tiles from home.
    pub fn dir(self) -> (i32, i32) {
        match self {
            SpokeAxis::EW => (1, 0),
            SpokeAxis::NS => (0, 1),
            SpokeAxis::NeSw => (1, -1),
            SpokeAxis::NwSe => (1, 1),
        }
    }

    /// Perpendicular axis used for parallel-street offsets and cross
    /// streets. (-dy, dx) rotates 90° counter-clockwise.
    pub fn perp(self) -> SpokeAxis {
        match self {
            SpokeAxis::EW => SpokeAxis::NS,
            SpokeAxis::NS => SpokeAxis::EW,
            SpokeAxis::NeSw => SpokeAxis::NwSe,
            SpokeAxis::NwSe => SpokeAxis::NeSw,
        }
    }
}

/// Discretise a (dx, dy) offset to one of the four axes. Pure helper
/// for axis derivation; never invents a primary axis on `(0, 0)` —
/// callers should fall through to the next priority instead.
fn classify_axis(dx: i32, dy: i32) -> SpokeAxis {
    let ax = dx.abs();
    let ay = dy.abs();
    if ax == 0 && ay == 0 {
        return SpokeAxis::EW;
    }
    let major = ax.max(ay) as f32;
    let minor = ax.min(ay) as f32;
    // ratio < 0.4 → strongly cardinal; otherwise call it diagonal.
    if minor / major < 0.4 {
        if ax >= ay {
            SpokeAxis::EW
        } else {
            SpokeAxis::NS
        }
    } else if dx.signum() == dy.signum() {
        SpokeAxis::NwSe
    } else {
        SpokeAxis::NeSw
    }
}

/// Derive the primary street axis for a settlement. Priority order:
/// 1. Parallel to a nearby river.
/// 2. Toward the dominant external anchor (gate, market spur, distant high-weight anchor).
/// 3. Largest eigen-direction of member-cluster offsets from home.
/// 4. Cardinal E-W fallback.
fn primary_axis<F: SurveyFactionView>(
    faction: &F,
    brain: &SettlementBrain,
    chunk_map: &ChunkMap,
    member_offsets: &[(i32, i32)],
) -> SpokeAxis {
    let home = faction.home_tile();

    // 1. River — already classified by river_context.
    if let Some(river_axis) =
        crate::simulation::river_context::river_orientation_near(chunk_map, home, 10)
    {
        use crate::simulation::river_context::RiverAxis;
        return match river_axis {
            RiverAxis::EW => SpokeAxis::EW,
            RiverAxis::NS => SpokeAxis::NS,
            RiverAxis::NeSw => SpokeAxis::NeSw,
            RiverAxis::NwSe => SpokeAxis::NwSe,
        };
    }

    // 2. Highest-weight anchor at chebyshev ≥ 10 — captures gates, market spurs,
    //    and water-access points that pull a primary street toward them.
    let mut external: Option<(f32, (i32, i32))> = None;
    for anchor in &brain.anchors {
        if matches!(anchor.kind, SettlementAnchorKind::CivicCore) {
            continue;
        }
        if cheb(anchor.tile, home) < 10 {
            continue;
        }
        if external.map_or(true, |(w, _)| anchor.weight > w) {
            external = Some((anchor.weight, anchor.tile));
        }
    }
    if let Some((_, tile)) = external {
        return classify_axis(tile.0 - home.0, tile.1 - home.1);
    }

    // 3. Member-cluster principal axis (PCA-lite over dx/dy offsets).
    let mut sxx = 0.0f32;
    let mut syy = 0.0f32;
    let mut sxy = 0.0f32;
    let mut n = 0.0f32;
    for &(dx, dy) in member_offsets {
        let fx = dx as f32;
        let fy = dy as f32;
        sxx += fx * fx;
        syy += fy * fy;
        sxy += fx * fy;
        n += 1.0;
    }
    if n >= 2.0 {
        let trace = sxx + syy;
        let det = sxx * syy - sxy * sxy;
        let disc = (trace * trace / 4.0 - det).max(0.0).sqrt();
        let lambda = trace / 2.0 + disc;
        let vx = lambda - syy;
        let vy = sxy;
        if vx.abs() + vy.abs() > 0.01 {
            return classify_axis(vx.round() as i32, vy.round() as i32);
        }
    }

    // 4. Cardinal fallback — only when no signal at all.
    SpokeAxis::EW
}

/// Lay a straight street segment of length ~`half_extent * 2` centred on
/// `centre` along `axis`. Returns endpoints suitable for `StreetSegment`.
fn line_through(centre: (i32, i32), axis: SpokeAxis, half_extent: i32) -> StreetSegment {
    let (dx, dy) = axis.dir();
    StreetSegment {
        start: (centre.0 - dx * half_extent, centre.1 - dy * half_extent),
        end: (centre.0 + dx * half_extent, centre.1 + dy * half_extent),
        tier: StreetTier::Primary,
    }
}

fn build_road_network<F: SurveyFactionView>(
    faction: &F,
    brain: &SettlementBrain,
    chunk_map: &ChunkMap,
    member_offsets: &[(i32, i32)],
) -> Vec<StreetSegment> {
    if !faction.community_has(PERM_SETTLEMENT) {
        return Vec::new();
    }
    if matches!(brain.phase, SettlementPhase::Camp) {
        return Vec::new();
    }

    let home = faction.home_tile();
    let radius = road_network_radius(brain.phase);
    if radius <= 0 {
        return Vec::new();
    }
    let has_bridges = faction.community_has(BRIDGE_BUILDING);
    let axis = primary_axis(faction, brain, chunk_map, member_offsets);
    let perp = axis.perp();
    let (ax, ay) = axis.dir();
    let (px, py) = perp.dir();

    let mut segments = Vec::new();

    // Primary spine through home along the derived axis.
    push_unique_segment(&mut segments, line_through(home, axis, radius));

    // Phase-scaled additions. Minimum spacing between parallel streets is
    // 12 tiles (≈ 18 m at 1.5 m/tile) — tight enough for one row of 3×3
    // huts + yards between two parallel streets, anything tighter wouldn't
    // fit a hut.
    //
    // Settlement realism: Village + Chiefdom additions are now anchor-
    // demand-driven instead of blind geometric crosses/parallels. The
    // spine remains; perpendiculars and secondaries only fire when an
    // off-spine anchor actually exists.
    match brain.phase {
        SettlementPhase::Camp | SettlementPhase::Hamlet => {
            // Spine only. No grid. Lets a hamlet whose only purpose is its
            // connection to a parent city run a single spine that direction.
        }
        SettlementPhase::Village => {
            // Conditions for a perpendicular street:
            //   (a) member_count ≥ 16 (a dense village wants the cross), OR
            //   (b) a WaterAccess / Field / Market anchor projects off the
            //       spine axis by > 6 tiles — there is a real off-axis
            //       destination people walk toward.
            let dense = faction.member_count() >= 16;
            // Strongest off-axis anchor (by |perp projection|).
            let mut best_off_axis: Option<(f32, i32, (i32, i32))> = None;
            for anchor in &brain.anchors {
                if !matches!(
                    anchor.kind,
                    SettlementAnchorKind::WaterAccess
                        | SettlementAnchorKind::Field
                        | SettlementAnchorKind::Market
                ) {
                    continue;
                }
                let dx = anchor.tile.0 - home.0;
                let dy = anchor.tile.1 - home.1;
                let perp_proj = dx * px + dy * py;
                let abs_proj = perp_proj.abs();
                if abs_proj <= 6 {
                    continue;
                }
                if best_off_axis.map_or(true, |(w, _, _)| anchor.weight > w) {
                    best_off_axis = Some((anchor.weight, perp_proj, anchor.tile));
                }
            }
            if dense || best_off_axis.is_some() {
                // Endpoint: the projection of the strongest off-spine
                // anchor (asymmetric), or the symmetric spine length when
                // density alone triggers the cross.
                let cross = if let Some((_, proj, _)) = best_off_axis {
                    let len = proj.abs().clamp(8, radius);
                    let sign = proj.signum();
                    StreetSegment {
                        start: home,
                        end: (home.0 + px * sign * len, home.1 + py * sign * len),
                        tier: StreetTier::Primary,
                    }
                } else {
                    line_through(home, perp, radius)
                };
                push_unique_segment(&mut segments, cross);
            }
        }
        SettlementPhase::Chiefdom => {
            // Top-3 unmet anchors (weight-sorted, dedup-against-spine).
            // Each emits a Secondary segment from `home` toward the
            // anchor's tile. No symmetric ±18 parallels.
            let spine_axis = axis;
            let already_on_spine = |t: (i32, i32)| -> bool {
                let dx = t.0 - home.0;
                let dy = t.1 - home.1;
                let perp_proj = dx * px + dy * py;
                // "On spine" = perpendicular projection within 2 tiles.
                perp_proj.abs() <= 2 && spine_axis as u8 != SpokeAxis::EW as u8
                    || perp_proj.abs() <= 2
            };
            let mut scored: Vec<(f32, (i32, i32))> = Vec::new();
            for anchor in &brain.anchors {
                if matches!(anchor.kind, SettlementAnchorKind::CivicCore) {
                    continue;
                }
                if cheb(anchor.tile, home) < 4 {
                    continue;
                }
                if already_on_spine(anchor.tile) {
                    continue;
                }
                if !has_bridges
                    && !crate::simulation::river_context::same_bank_bfs(
                        chunk_map,
                        home,
                        anchor.tile,
                    )
                {
                    continue;
                }
                scored.push((anchor.weight, anchor.tile));
            }
            scored.sort_by(|a, b| {
                b.0.partial_cmp(&a.0)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.1.cmp(&b.1))
            });
            for (_, target) in scored.into_iter().take(3) {
                push_unique_segment(
                    &mut segments,
                    StreetSegment {
                        start: home,
                        end: target,
                        tier: StreetTier::Secondary,
                    },
                );
            }
        }
        SettlementPhase::ProtoUrban => {
            for off in [-30, -15, 15, 30] {
                let mid = (home.0 + px * off, home.1 + py * off);
                let mut seg = line_through(mid, axis, radius);
                seg.tier = StreetTier::Secondary;
                push_unique_segment(&mut segments, seg);
            }
            for off in [-12, 0, 12] {
                let mid = (home.0 + ax * off, home.1 + ay * off);
                let mut seg = line_through(mid, perp, radius);
                seg.tier = StreetTier::Secondary;
                push_unique_segment(&mut segments, seg);
            }
        }
        SettlementPhase::Urban => {
            for off in [-24, -12, 12, 24] {
                let mid = (home.0 + px * off, home.1 + py * off);
                let mut seg = line_through(mid, axis, radius);
                seg.tier = StreetTier::Secondary;
                push_unique_segment(&mut segments, seg);
            }
            for off in [-18, -9, 0, 9, 18] {
                let mid = (home.0 + ax * off, home.1 + ay * off);
                let mut seg = line_through(mid, perp, radius);
                seg.tier = StreetTier::Secondary;
                push_unique_segment(&mut segments, seg);
            }
        }
    }

    // Anchor/desire-path segments — append after the grid so they connect
    // into the base network rather than competing for primary position.
    let mut endpoints: Vec<(f32, (i32, i32), StreetTier)> = Vec::new();
    for anchor in &brain.anchors {
        let tier = match anchor.kind {
            SettlementAnchorKind::WaterAccess
            | SettlementAnchorKind::Field
            | SettlementAnchorKind::Market
            | SettlementAnchorKind::Gate => StreetTier::Primary,
            _ => StreetTier::Secondary,
        };
        if cheb(anchor.tile, home) >= 4 {
            // Pre-bridge: only consider anchors reachable on the same bank.
            // Post-bridge: take any anchor — the emitter will back-fill
            // crossings short enough for `MAX_BRIDGE_SPAN`.
            if !has_bridges
                && !crate::simulation::river_context::same_bank_bfs(chunk_map, home, anchor.tile)
            {
                continue;
            }
            endpoints.push((anchor.weight, anchor.tile, tier));
        }
    }
    for (&tile, &heat) in &brain.traffic_heat {
        if heat >= 80 && cheb(tile, home) >= 4 {
            if !has_bridges
                && !crate::simulation::river_context::same_bank_bfs(chunk_map, home, tile)
            {
                continue;
            }
            endpoints.push((heat as f32 / 255.0, tile, StreetTier::Alley));
        }
    }
    endpoints.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.cmp(&b.1))
    });
    for (_, end, tier) in endpoints.into_iter().take(8) {
        push_unique_segment(
            &mut segments,
            StreetSegment {
                start: home,
                end,
                tier,
            },
        );
    }

    // Settlement realism: jitter Secondary / Alley endpoints by ±1 tile
    // keyed on `layout_hash` so anchor spurs aren't perfect cardinals.
    // Primary spine endpoints stay verbatim — every door doglegs back to
    // the spine via the connector helper and a jittered primary would
    // both break that contract and produce broken-stub tests.
    for (i, seg) in segments.iter_mut().enumerate() {
        if matches!(seg.tier, StreetTier::Primary) {
            continue;
        }
        let h = brain.layout_hash ^ ((i as u64) << 16);
        let jx = ((h ^ 0xA5A5_A5A5) >> 8) as i32 % 3 - 1; // {-1,0,1}
        let jy = ((h ^ 0x5A5A_5A5A) >> 16) as i32 % 3 - 1;
        if seg.start != home {
            seg.start.0 += jx;
            seg.start.1 += jy;
        }
        let jx2 = ((h ^ 0xC3C3_C3C3) >> 24) as i32 % 3 - 1;
        let jy2 = ((h ^ 0x3C3C_3C3C) >> 32) as i32 % 3 - 1;
        if seg.end != home {
            seg.end.0 += jx2;
            seg.end.1 += jy2;
        }
    }

    // Pre-bridge: drop any segment whose Bresenham trace touches a River
    // tile. Without bridges, `road_carve_system` would silently skip the
    // wet cells and leave disconnected stubs — better to never plan them.
    // Post-bridge: keep crossings; `bridge_intent_emitter_system` walks
    // these same segments and back-fills short runs with Bridge blueprints.
    if !has_bridges {
        segments.retain(|seg| !trace_crosses_river(chunk_map, *seg));
    }

    segments
}

fn trace_crosses_river(chunk_map: &ChunkMap, seg: StreetSegment) -> bool {
    for (tx, ty) in bresenham_tiles(seg.start, seg.end) {
        if matches!(
            chunk_map.tile_kind_at(tx, ty),
            Some(crate::world::tile::TileKind::River)
        ) {
            return true;
        }
    }
    false
}

pub(crate) fn road_network_radius(phase: SettlementPhase) -> i32 {
    match phase {
        SettlementPhase::Camp => 0,
        SettlementPhase::Hamlet => 10,
        SettlementPhase::Village => 16,
        SettlementPhase::Chiefdom => 22,
        SettlementPhase::ProtoUrban => 30,
        SettlementPhase::Urban => 36,
    }
}

fn push_unique_segment(segments: &mut Vec<StreetSegment>, segment: StreetSegment) {
    if segment.start == segment.end {
        return;
    }
    let duplicate = segments.iter().any(|existing| {
        (existing.start == segment.start && existing.end == segment.end)
            || (existing.start == segment.end && existing.end == segment.start)
    });
    if !duplicate {
        segments.push(segment);
    }
}

fn road_tiles_for_segments(segments: &[StreetSegment]) -> AHashSet<(i32, i32)> {
    let mut tiles = AHashSet::default();
    for segment in segments {
        for tile in bresenham_tiles(segment.start, segment.end) {
            if tile == segment.start || tile == segment.end {
                continue;
            }
            tiles.insert(tile);
        }
    }
    tiles
}

/// Compute the perpendicular widening offset for a single Bresenham segment.
/// Horizontal-ish runs widen with a +1 row; vertical-ish runs with a +1
/// column. Mirrors the rule in `construction::road_carve_system`; single
/// source of truth.
#[inline]
pub(crate) fn road_widen_offset(from: (i32, i32), to: (i32, i32)) -> (i32, i32) {
    let dx_abs = (to.0 - from.0).abs();
    let dy_abs = (to.1 - from.1).abs();
    if dy_abs > dx_abs {
        (1, 0)
    } else {
        (0, 1)
    }
}

/// Resolve the actual widened tile for one centerline cell so the 2-tile road
/// corridor *routes around* a standing structure instead of running through it.
/// The widen **axis** is per-segment (`road_widen_offset`); only the **sign**
/// is adaptive per tile: prefer the default side, flip to the opposite side
/// when the default neighbour is blocked, and fall back to the default when
/// both sides are blocked (the carver's `StructureIndex` backstop then skips
/// that one tile rather than overwrite a building). Pass `|_| false` for the
/// unconditional baseline used by tests/fixtures.
#[inline]
pub(crate) fn road_widen_tile(
    centerline: (i32, i32),
    from: (i32, i32),
    to: (i32, i32),
    is_blocked: impl Fn((i32, i32)) -> bool,
) -> (i32, i32) {
    let (ox, oy) = road_widen_offset(from, to);
    let default = (centerline.0 + ox, centerline.1 + oy);
    if !is_blocked(default) {
        return default;
    }
    let opposite = (centerline.0 - ox, centerline.1 - oy);
    if !is_blocked(opposite) {
        return opposite;
    }
    default
}

/// Road corridor (centerline + perpendicular widening tile) for the given
/// segments. Matches what `road_carve_system` actually stamps, so the planner
/// can reject parcel rects that the carver would otherwise clip. The widening
/// routes around `is_blocked` tiles per cell (see `road_widen_tile`).
pub(crate) fn road_corridor_tiles_for_segments_with(
    segments: &[StreetSegment],
    is_blocked: impl Fn((i32, i32)) -> bool,
) -> AHashSet<(i32, i32)> {
    let mut tiles = AHashSet::default();
    for segment in segments {
        for tile in bresenham_tiles(segment.start, segment.end) {
            if tile == segment.start || tile == segment.end {
                continue;
            }
            tiles.insert(tile);
            tiles.insert(road_widen_tile(tile, segment.start, segment.end, &is_blocked));
        }
    }
    tiles
}

/// Unconditional baseline (default widen side, no structure avoidance).
pub(crate) fn road_corridor_tiles_for_segments(
    segments: &[StreetSegment],
) -> AHashSet<(i32, i32)> {
    road_corridor_tiles_for_segments_with(segments, |_| false)
}

/// Footprint tiles a `ConstructionIntent` would occupy — mirror of
/// `construction::candidate_footprint_tiles` for the organic intent enum.
fn organic_intent_footprint_tiles(kind: OrganicBuildKind, tile: (i32, i32)) -> Vec<(i32, i32)> {
    match kind {
        OrganicBuildKind::Single(_) | OrganicBuildKind::PalisadeSegment(_) => vec![tile],
        OrganicBuildKind::Hut(_) => rect_tiles_inclusive(tile, 1, 1),
        OrganicBuildKind::Longhouse { axis, .. } => {
            let (hw, hh) = axis.longhouse_halves();
            rect_tiles_inclusive(tile, hw, hh)
        }
        OrganicBuildKind::CompositeHouse {
            shape, rotation, ..
        } => crate::simulation::building_template::shape_tiles(shape, tile, rotation),
    }
}

fn rect_tiles_inclusive(centre: (i32, i32), half_w: i32, half_h: i32) -> Vec<(i32, i32)> {
    let mut out = Vec::new();
    for dy in -half_h..=half_h {
        for dx in -half_w..=half_w {
            out.push((centre.0 + dx, centre.1 + dy));
        }
    }
    out
}

/// Rasterise every queued-but-uncarved road segment for `faction_id` into a
/// reservation set, using the same adaptive 2-tile widen the carver applies.
/// Built once per faction so the per-candidate guard stays cheap.
fn queued_road_reservation_for_faction(
    road_queue: &RoadCarveQueue,
    structure_index: &crate::simulation::construction::StructureIndex,
    faction_id: u32,
) -> crate::simulation::seed_reservation::SeedReservation {
    let mut queued = crate::simulation::seed_reservation::SeedReservation::default();
    for &(fid, from, to) in road_queue.0.iter() {
        if fid != faction_id {
            continue;
        }
        crate::simulation::seed_reservation::rasterize_line_into(&mut queued, from, to, |t| {
            structure_index.0.contains_key(&t)
        });
    }
    queued
}

/// Selection-time reserved-road guard: true when an intent's footprint touches
/// the planned widened corridor, an already-carved road, or a queued-but-
/// uncarved corridor (`queued`). Mirrors
/// `construction::candidate_touches_reserved_road` so the planner never
/// *selects* an intent the directive layer would then silently drop (which
/// would starve every lower-priority intent that tick).
fn intent_touches_reserved_road(
    kind: OrganicBuildKind,
    tile: (i32, i32),
    brain: &SettlementBrain,
    queued: &crate::simulation::seed_reservation::SeedReservation,
    chunk_map: &ChunkMap,
) -> bool {
    let fp = organic_intent_footprint_tiles(kind, tile);
    if fp.iter().any(|t| brain.road_corridor_tiles.contains(t)) {
        return true;
    }
    if fp
        .iter()
        .any(|t| chunk_map.tile_kind_at(t.0, t.1) == Some(crate::world::tile::TileKind::Road))
    {
        return true;
    }
    fp.iter().any(|t| queued.is_reserved(*t))
}

fn bresenham_tiles(from: (i32, i32), to: (i32, i32)) -> Vec<(i32, i32)> {
    let mut out = Vec::new();
    let (mut x0, mut y0) = from;
    let (x1, y1) = to;
    let dx = (x1 - x0).abs();
    let dy = -(y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;

    loop {
        out.push((x0, y0));
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
    out
}

fn build_frontier<F: SurveyFactionView>(
    faction: &F,
    brain: &SettlementBrain,
    chunk_map: &ChunkMap,
    maps: &SurveyStructureSnapshot,
) -> Vec<(i32, i32)> {
    let home = faction.home_tile();
    let radius = survey_radius(brain.phase);
    let road_centred = faction.community_has(PERM_SETTLEMENT) && !brain.road_tiles.is_empty();
    let mut scored: Vec<(f32, i32, i32)> = Vec::new();
    for y in home.1 - radius..=home.1 + radius {
        for x in home.0 - radius..=home.0 + radius {
            let tile = (x, y);
            if cheb(tile, home) > radius {
                continue;
            }
            if !tile_open_for_frontier(chunk_map, maps, tile) {
                continue;
            }
            let road_dist = distance_to_road_network(chunk_map, brain, tile, 6);
            if road_centred && road_dist.is_none() {
                continue;
            }
            if road_dist == Some(0) || brain.road_tiles.contains(&tile) {
                continue;
            }
            let mut score = frontier_score(faction, brain, chunk_map, tile);
            if let Some(d) = road_dist {
                score += (6 - d).max(0) as f32 * 1.75;
            }
            if score <= 0.0 {
                continue;
            }
            scored.push((score, x, y));
        }
    }
    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.2.cmp(&b.2))
    });
    scored
        .into_iter()
        .take(MAX_FRONTIER)
        .map(|(_, x, y)| (x, y))
        .collect()
}

fn build_parcels<F: SurveyFactionView>(
    faction: &F,
    settlement: &Settlement,
    brain: &SettlementBrain,
    chunk_map: &ChunkMap,
    maps: &SurveyStructureSnapshot,
    committed_ag_rects: &[TileRect],
) -> Vec<Parcel> {
    // Permanent settlements with a road skeleton: sweep road tiles for
    // road-fronted parcel candidates. Camps & nomadic factions (no
    // PERM_SETTLEMENT) and survey-gap settlements (road network not yet
    // sketched) fall back to the legacy frontier-first allocation.
    if faction.community_has(PERM_SETTLEMENT) && !brain.road_tiles.is_empty() {
        // Non-ag parcels via the road sweep, then the agricultural belt
        // OUTSIDE the resulting built-up footprint. One combined Vec with a
        // continuous id sequence; the belt shares the `MAX_PARCELS` budget.
        let mut parcels = build_parcels_road_driven(faction, settlement, brain, chunk_map);
        let occupied: Vec<TileRect> = parcels.iter().map(|p| p.rect()).collect();
        let next_id = parcels
            .iter()
            .map(|p| p.id)
            .max()
            .map(|m| m.wrapping_add(1))
            .unwrap_or(0);
        let budget = MAX_PARCELS.saturating_sub(parcels.len());
        let belt = build_ag_belt(
            faction,
            settlement,
            brain,
            chunk_map,
            &occupied,
            next_id,
            budget,
            committed_ag_rects,
        );
        parcels.extend(belt);
        // Per-dwelling kitchen-garden pass: emit a house-sized Agricultural
        // parcel beside every walled dwelling that has no belt parcel within
        // `KITCHEN_PROXIMITY` tiles. Keyed on actual built dwellings (detected
        // from the wall snapshot) so garden geometry matches the house. The
        // 12-tile child-claim path in `land_listing_system` then binds it as
        // the household's child plot; carving still flows through
        // `carve_plots_system` — no new Plot construction path.
        append_dwelling_gardens(faction, settlement, brain, chunk_map, &maps.walls, &mut parcels);
        parcels
    } else {
        build_parcels_frontier_driven(faction, settlement, brain, chunk_map)
    }
}

/// A walled dwelling detected from the survey wall snapshot.
struct DetectedDwelling {
    /// Full wall-ring footprint (3×3 hut / 5×3 or 3×5 longhouse).
    rect: TileRect,
    is_longhouse: bool,
}

/// Rectangle perimeter (wall-ring) tiles.
fn rect_perimeter_tiles(r: TileRect) -> Vec<(i32, i32)> {
    let (w, h) = (r.w as i32, r.h as i32);
    let mut v = Vec::new();
    for y in r.y0..r.y0 + h {
        for x in r.x0..r.x0 + w {
            if x == r.x0 || x == r.x0 + w - 1 || y == r.y0 || y == r.y0 + h - 1 {
                v.push((x, y));
            }
        }
    }
    v
}

/// Detect Hut (3×3) and Longhouse (5×3 / 3×5) walled houses from the wall
/// snapshot: an axis-aligned ring of wall tiles with a single door gap and a
/// wall-free interior. Palisades and civic structures don't form such rings.
fn detect_dwellings(walls: &AHashSet<(i32, i32)>) -> Vec<DetectedDwelling> {
    // Longhouse shapes first so a longhouse corner isn't mis-claimed as a hut.
    const SHAPES: [(u16, u16, bool); 3] = [(5, 3, true), (3, 5, true), (3, 3, false)];
    let mut sorted: Vec<(i32, i32)> = walls.iter().copied().collect();
    sorted.sort_unstable();
    let mut found: Vec<DetectedDwelling> = Vec::new();
    let mut claimed: AHashSet<(i32, i32)> = AHashSet::default();
    for (x0, y0) in sorted {
        for (w, h, is_long) in SHAPES {
            let rect = TileRect::new(x0, y0, w, h);
            let perim = rect_perimeter_tiles(rect);
            if perim.iter().any(|t| claimed.contains(t)) {
                continue;
            }
            let wall_hits = perim.iter().filter(|t| walls.contains(t)).count();
            // A single door gap is allowed; everything else must be wall.
            if wall_hits + 1 < perim.len() {
                continue;
            }
            let interior_clear = (y0 + 1..y0 + h as i32 - 1)
                .all(|y| (x0 + 1..x0 + w as i32 - 1).all(|x| !walls.contains(&(x, y))));
            if !interior_clear {
                continue;
            }
            for t in &perim {
                claimed.insert(*t);
            }
            found.push(DetectedDwelling {
                rect,
                is_longhouse: is_long,
            });
            break;
        }
    }
    found
}

/// Emits a personal kitchen-garden Agricultural parcel beside every walled
/// dwelling with no belt parcel within `KITCHEN_PROXIMITY` chebyshev tiles.
/// Garden geometry matches the dwelling: a 3×3 Hut garden attaches to any
/// clear edge; a 3×4 Longhouse garden attaches only to a 3-tile short wall
/// (never the 5-tile long wall) and is skipped if neither short side is clear
/// and reachable. The 12-tile child-claim path in `land_listing_system` then
/// binds the parcel as the household's child plot; carving still flows
/// through `carve_plots_system` — no new Plot construction path.
fn append_dwelling_gardens<F: SurveyFactionView>(
    faction: &F,
    settlement: &Settlement,
    brain: &SettlementBrain,
    chunk_map: &ChunkMap,
    walls: &AHashSet<(i32, i32)>,
    parcels: &mut Vec<Parcel>,
) {
    const KITCHEN_PROXIMITY: i32 = 12;
    const BELT_CLEARANCE: i32 = 3;

    let dwellings = detect_dwellings(walls);
    if dwellings.is_empty() {
        return;
    }
    let belt_centres: Vec<(i32, i32)> = parcels
        .iter()
        .filter(|p| p.district_hint == Some(DistrictKind::Agricultural))
        .map(|p| p.centre())
        .collect();

    let inflate = |r: TileRect| -> TileRect {
        TileRect::new(
            r.x0 - BELT_CLEARANCE,
            r.y0 - BELT_CLEARANCE,
            r.w + 2 * BELT_CLEARANCE as u16,
            r.h + 2 * BELT_CLEARANCE as u16,
        )
    };
    // Overlap keep-out: every NON-residential parcel plus Civic/Crafting
    // district discs, inflated. Residential parcels and the Residential
    // district disc are deliberately excluded — a personal garden belongs in
    // the residential area and may abut its owning dwelling's wall.
    // `rect_clear_for_parcel` still rejects wall tiles, so a garden can never
    // overlap a built neighbour house.
    let mut footprint: Vec<TileRect> = parcels
        .iter()
        .filter(|p| p.district_hint != Some(DistrictKind::Residential))
        .map(|p| inflate(p.rect()))
        .collect();
    for d in &brain.districts {
        if matches!(d.kind, DistrictKind::Civic | DistrictKind::Crafting) {
            let r = d.radius as i32;
            footprint.push(inflate(TileRect::new(
                d.centre.0 - r,
                d.centre.1 - r,
                (2 * r + 1) as u16,
                (2 * r + 1) as u16,
            )));
        }
    }

    let home = faction.home_tile();
    let mut next_id = parcels
        .iter()
        .map(|p| p.id)
        .max()
        .map(|m| m.wrapping_add(1))
        .unwrap_or(0);
    let budget = MAX_PARCELS.saturating_sub(parcels.len());
    let mut emitted = 0usize;

    for dwelling in dwellings {
        if emitted >= budget {
            break;
        }
        let centre = dwelling.rect.center();
        let near_belt = belt_centres
            .iter()
            .any(|b| cheb(*b, centre) <= KITCHEN_PROXIMITY);
        if near_belt {
            continue;
        }
        // Prefer the side away from home / the settlement core.
        let away = opposite_edge(TileEdge::toward(centre, home));
        let candidates: Vec<TileEdge> = if dwelling.is_longhouse {
            // Short walls are the pair perpendicular to the long axis.
            let pair = if dwelling.rect.w >= dwelling.rect.h {
                [TileEdge::East, TileEdge::West]
            } else {
                [TileEdge::North, TileEdge::South]
            };
            if pair.contains(&away) {
                vec![away, opposite_edge(away)]
            } else {
                pair.to_vec()
            }
        } else {
            vec![away, rotate_edge_cw(away), rotate_edge_ccw(away)]
        };
        let depth: u16 = if dwelling.is_longhouse { 4 } else { 3 };

        let mut chosen: Option<TileRect> = None;
        for edge in candidates {
            let rect = kitchen_rect_for_edge(dwelling.rect, edge, depth);
            if !rect_clear_for_parcel(chunk_map, rect, &brain.road_corridor_tiles) {
                continue;
            }
            if footprint.iter().any(|f| rects_overlap(*f, rect)) {
                continue;
            }
            let max = (rect.x0 + rect.w as i32 - 1, rect.y0 + rect.h as i32 - 1);
            if !crate::simulation::placement_reachability::rect_reachable_from_home(
                chunk_map,
                home,
                (rect.x0, rect.y0),
                max,
            ) {
                continue;
            }
            chosen = Some(rect);
            break;
        }
        let Some(rect) = chosen else { continue };
        let suitability = parcel_suitability(faction, settlement, brain, chunk_map, rect.center());
        parcels.push(Parcel {
            id: next_id,
            shape: ParcelShape::Rect(rect),
            frontage_edge: None,
            access_tile: None,
            holder: TenureHolder::State {
                faction_id: settlement.owner_faction,
            },
            district_hint: Some(DistrictKind::Agricultural),
            suitability,
        });
        // Track so a later dwelling's garden doesn't overlap this one.
        footprint.push(inflate(rect));
        next_id = next_id.wrapping_add(1);
        emitted += 1;
    }
}

fn opposite_edge(e: TileEdge) -> TileEdge {
    match e {
        TileEdge::North => TileEdge::South,
        TileEdge::South => TileEdge::North,
        TileEdge::East => TileEdge::West,
        TileEdge::West => TileEdge::East,
    }
}

fn rotate_edge_cw(e: TileEdge) -> TileEdge {
    match e {
        TileEdge::North => TileEdge::East,
        TileEdge::East => TileEdge::South,
        TileEdge::South => TileEdge::West,
        TileEdge::West => TileEdge::North,
    }
}

fn rotate_edge_ccw(e: TileEdge) -> TileEdge {
    match e {
        TileEdge::North => TileEdge::West,
        TileEdge::West => TileEdge::South,
        TileEdge::South => TileEdge::East,
        TileEdge::East => TileEdge::North,
    }
}

/// Garden rect flush against `house`'s `edge`, extending `depth` tiles
/// outward. The wall-parallel dimension equals the house wall's length so the
/// shared edge runs 1:1 with the wall — no centring offset.
fn kitchen_rect_for_edge(house: TileRect, edge: TileEdge, depth: u16) -> TileRect {
    let hw = house.w;
    let hh = house.h;
    let d = depth as i32;
    match edge {
        TileEdge::North => TileRect::new(house.x0, house.y0 + hh as i32, hw, depth),
        TileEdge::South => TileRect::new(house.x0, house.y0 - d, hw, depth),
        TileEdge::East => TileRect::new(house.x0 + hw as i32, house.y0, depth, hh),
        TileEdge::West => TileRect::new(house.x0 - d, house.y0, depth, hh),
    }
}

/// Frontier-first parcel allocation. Used for camps and nomadic factions
/// that don't run the road sweep. Identical to the historical algorithm.
fn build_parcels_frontier_driven<F: SurveyFactionView>(
    faction: &F,
    settlement: &Settlement,
    brain: &SettlementBrain,
    chunk_map: &ChunkMap,
) -> Vec<Parcel> {
    let mut parcels = Vec::new();
    let mut occupied: Vec<TileRect> = Vec::new();
    let mut counts: AHashMap<DistrictKind, usize> = AHashMap::default();
    let targets = parcel_targets(
        faction,
        settlement,
        brain.phase,
        current_ag_tile_count(brain),
    );
    let road_centred = faction.community_has(PERM_SETTLEMENT) && !brain.road_tiles.is_empty();
    let mut next_id = 0u32;
    for &tile in &brain.frontier {
        if parcels.len() >= MAX_PARCELS {
            break;
        }
        let suitability = parcel_suitability(faction, settlement, brain, chunk_map, tile);
        let Some(kind) = choose_district_for_parcel(&suitability, &targets, &counts) else {
            continue;
        };
        let (w, h) = parcel_size(kind, brain.phase);
        let rect = TileRect::new(tile.0 - w as i32 / 2, tile.1 - h as i32 / 2, w, h);
        if occupied.iter().any(|r| rects_overlap(*r, rect)) {
            continue;
        }
        if !rect_clear_for_parcel(chunk_map, rect, &brain.road_corridor_tiles) {
            continue;
        }
        let (frontage_edge, access_tile) = frontage_to_network(chunk_map, brain, rect);
        if road_centred && frontage_edge.is_none() {
            continue;
        }
        parcels.push(Parcel {
            id: next_id,
            shape: ParcelShape::Rect(rect),
            frontage_edge,
            access_tile,
            holder: TenureHolder::State {
                faction_id: settlement.owner_faction,
            },
            district_hint: Some(kind),
            suitability,
        });
        occupied.push(rect);
        *counts.entry(kind).or_insert(0) += 1;
        next_id = next_id.wrapping_add(1);
    }
    parcels
}

/// Ideal-distance band (min, ideal, max) in chebyshev tiles from home for a
/// district at the given phase. Bands replace the old monotonic
/// `1/(1 + home_dist·0.05)` proximity bias so each district lands in its own
/// ring (civic clings to commons, defense rings the outer road, residential
/// fills the middle), preventing the visible center-clump.
///
/// `ideal` scales with the road radius across phases, so a Hamlet gets a
/// tighter layout than an Urban settlement automatically.
pub(crate) fn ideal_distance_band(
    district: DistrictKind,
    phase: SettlementPhase,
) -> Option<(i32, i32, i32)> {
    let cr = commons_radius(phase);
    let rr = road_network_radius(phase);
    if rr == 0 {
        return None;
    }
    // Phase scaling factor — `ideal` for "core" districts is proportional to
    // road radius. The 0.40 / 0.55 / 0.85 anchors are picked so Village
    // (rr=16) → ideal Residential ~6, Crafting/Market/Sacred ~9, Defense ~14.
    let ideal_at = |frac: f32| -> i32 {
        let v = (rr as f32 * frac).round() as i32;
        v.max(cr + 1)
    };
    match district {
        DistrictKind::Civic => Some((0, cr.max(1), (cr + 2).max(2))),
        DistrictKind::Storage => Some((cr + 1, (cr + 3).max(ideal_at(0.30)), rr / 2 + 2)),
        DistrictKind::Residential => Some((cr + 2, ideal_at(0.40), (rr - 1).max(cr + 3))),
        DistrictKind::Crafting | DistrictKind::Market | DistrictKind::Sacred => {
            Some((cr + 4, ideal_at(0.55), rr))
        }
        DistrictKind::Defense => Some(((rr - 4).max(cr + 1), rr, rr + 2)),
        DistrictKind::Agricultural => None,
    }
}

/// Band multiplier for `(district, phase, dist-from-home)`: triangular peak at
/// `ideal`, falls to 0 at the band edges, hard 0 outside `[min, max]`.
pub(crate) fn band_mul(district: DistrictKind, phase: SettlementPhase, dist: i32) -> f32 {
    let Some((lo, ideal, hi)) = ideal_distance_band(district, phase) else {
        return 1.0;
    };
    if dist < lo || dist > hi {
        return 0.0;
    }
    let left = (ideal - lo).max(1) as f32;
    let right = (hi - ideal).max(1) as f32;
    let dev = (dist - ideal).abs() as f32;
    let denom = if dist <= ideal { left } else { right };
    (1.0 - dev / denom).clamp(0.0, 1.0)
}

/// Road-tile-driven parcel allocation. For each road tile, derives one
/// candidate rect per cardinal × district kind, scores by
/// `suitability × deficit × proximity`, and greedily accepts non-overlapping
/// rects. Every resulting parcel has a guaranteed `frontage_edge` +
/// `access_tile` because the rect's edge is by construction adjacent to a
/// road tile.
fn build_parcels_road_driven<F: SurveyFactionView>(
    faction: &F,
    settlement: &Settlement,
    brain: &SettlementBrain,
    chunk_map: &ChunkMap,
) -> Vec<Parcel> {
    let targets = parcel_targets(
        faction,
        settlement,
        brain.phase,
        current_ag_tile_count(brain),
    );
    if targets.is_empty() {
        return Vec::new();
    }

    let home = faction.home_tile();
    let mut road_tiles: Vec<(i32, i32)> = brain.road_tiles.iter().copied().collect();
    road_tiles.sort_by_key(|t| (cheb(*t, home), t.0, t.1));

    struct Cand {
        rect: TileRect,
        edge: TileEdge,
        access_tile: (i32, i32),
        kind: DistrictKind,
        suitability: ParcelSuitability,
        score: f32,
        home_dist: i32,
        tile_hash: u64,
    }
    let mut candidates: Vec<Cand> = Vec::new();

    // Agricultural is deliberately absent: fields are NOT road-fronted core
    // parcels. `build_ag_belt` allocates them as a contiguous belt OUTSIDE the
    // built-up footprint (see `build_parcels`).
    const KIND_ORDER: [DistrictKind; 7] = [
        DistrictKind::Residential,
        DistrictKind::Crafting,
        DistrictKind::Storage,
        DistrictKind::Civic,
        DistrictKind::Defense,
        DistrictKind::Sacred,
        DistrictKind::Market,
    ];
    const EDGES: [TileEdge; 4] = [
        TileEdge::North,
        TileEdge::East,
        TileEdge::South,
        TileEdge::West,
    ];

    for &road_tile in &road_tiles {
        for edge in EDGES {
            for kind in KIND_ORDER {
                let target = *targets.get(&kind).unwrap_or(&0);
                if target == 0 {
                    continue;
                }
                let (w, h) = parcel_size(kind, brain.phase);
                let rect = parcel_rect_from_road(road_tile, edge, w, h);
                if !rect_clear_for_parcel(chunk_map, rect, &brain.road_corridor_tiles) {
                    continue;
                }
                // L1: commons keepout — only Civic builds may stamp into the
                // home commons disc; everything else must keep clear.
                if !district_allowed_in_commons(kind)
                    && rect_intersects_commons(brain.commons_rect, rect)
                {
                    continue;
                }
                let centre = rect.center();
                let suit = parcel_suitability(faction, settlement, brain, chunk_map, centre);
                let s = suit.for_district(kind);
                let threshold = match kind {
                    DistrictKind::Residential | DistrictKind::Civic => 0.25,
                    DistrictKind::Agricultural => 0.35,
                    _ => 0.4,
                };
                if s < threshold {
                    continue;
                }
                let home_dist = cheb(centre, home);
                // L4: distance-band scoring. Hard reject outside the band;
                // multiply suitability by the triangular peak inside it.
                let band = band_mul(kind, brain.phase, home_dist);
                if band <= 0.0 {
                    continue;
                }
                let score = s * (target as f32) * band;
                let tile_hash = ((centre.0 as i64).wrapping_mul(0x9E37_79B9_7F4A_7C15u64 as i64)
                    ^ centre.1 as i64) as u64;
                candidates.push(Cand {
                    rect,
                    edge,
                    access_tile: road_tile,
                    kind,
                    suitability: suit,
                    score,
                    home_dist,
                    tile_hash,
                });
            }
        }
    }

    candidates.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.home_dist.cmp(&b.home_dist))
            .then_with(|| a.tile_hash.cmp(&b.tile_hash))
    });

    let mut parcels = Vec::new();
    let mut occupied: Vec<(TileRect, DistrictKind, (i32, i32))> = Vec::new();
    let mut counts: AHashMap<DistrictKind, usize> = AHashMap::default();
    let mut next_id = 0u32;
    for cand in candidates {
        if parcels.len() >= MAX_PARCELS {
            break;
        }
        let target = *targets.get(&cand.kind).unwrap_or(&0);
        let already = *counts.get(&cand.kind).unwrap_or(&0);
        if already >= target {
            continue;
        }
        // L5: 1-tile inter-parcel buffer for non-agricultural pairs, with a
        // shared-frontage carve-out (two parcels facing across the same road
        // segment legitimately share an edge — row-house geometry).
        if occupied.iter().any(|(r, k, access)| {
            parcels_conflict(
                cand.rect, cand.kind, cand.access_tile,
                *r, *k, *access,
            )
        }) {
            continue;
        }
        parcels.push(Parcel {
            id: next_id,
            shape: ParcelShape::Rect(cand.rect),
            frontage_edge: Some(cand.edge),
            access_tile: Some(cand.access_tile),
            holder: TenureHolder::State {
                faction_id: settlement.owner_faction,
            },
            district_hint: Some(cand.kind),
            suitability: cand.suitability,
        });
        occupied.push((cand.rect, cand.kind, cand.access_tile));
        *counts.entry(cand.kind).or_insert(0) += 1;
        next_id = next_id.wrapping_add(1);
    }
    parcels
}

/// Conflict predicate for non-agricultural parcel pairs. Agricultural pairs
/// use plain overlap (`build_ag_belt` already runs its own keepout). Non-ag
/// pairs require a 1-tile breathing gap unless both parcels share frontage
/// on the same road tile (legitimate row-house geometry across a road).
fn parcels_conflict(
    a_rect: TileRect,
    a_kind: DistrictKind,
    a_access: (i32, i32),
    b_rect: TileRect,
    b_kind: DistrictKind,
    b_access: (i32, i32),
) -> bool {
    if a_kind == DistrictKind::Agricultural || b_kind == DistrictKind::Agricultural {
        return rects_overlap(a_rect, b_rect);
    }
    let inflated = TileRect::new(
        a_rect.x0 - 1,
        a_rect.y0 - 1,
        a_rect.w + 2,
        a_rect.h + 2,
    );
    if !rects_overlap(inflated, b_rect) {
        return false;
    }
    // Hard overlap stays a conflict regardless of frontage sharing.
    if rects_overlap(a_rect, b_rect) {
        return true;
    }
    // Shared frontage carve-out: both access tiles are on adjacent or the
    // same road tile, and the inflated overlap is exactly the road strip.
    let shared_frontage =
        (a_access.0 - b_access.0).abs() <= 1 && (a_access.1 - b_access.1).abs() <= 1;
    !shared_frontage
}

/// Agricultural-belt allocation. Fields are deliberately NOT road-fronted
/// core parcels (that wove 16×16 farm blocks through the street grid). The
/// belt is packed as contiguous 16×16 blocks OUTSIDE the built-up footprint,
/// anchored on the Agricultural `DistrictInfluence` (the `Field` anchor from
/// `best_fertile_tile`), grown as a connected blob from the most fertile seed,
/// requiring only a short track to the road network (not full frontage).
///
/// Deterministic: fixed 16-aligned lattice, no RNG, explicit sort/selection
/// with a `tile_hash` final tiebreak, no ahash-iteration dependence. `occupied`
/// is the non-ag road-driven parcel set; `start_id` continues the parcel id
/// sequence; `budget` is the remaining `MAX_PARCELS` headroom.
#[allow(clippy::too_many_arguments)]
fn build_ag_belt<F: SurveyFactionView>(
    faction: &F,
    settlement: &Settlement,
    brain: &SettlementBrain,
    chunk_map: &ChunkMap,
    occupied: &[TileRect],
    start_id: u32,
    budget: usize,
    committed_ag_rects: &[TileRect],
) -> Vec<Parcel> {
    const BELT_CLEARANCE: i32 = 3;
    const ACCESS_SOFT: i32 = 12;

    if budget == 0 {
        return Vec::new();
    }
    let targets = parcel_targets(
        faction,
        settlement,
        brain.phase,
        current_ag_tile_count(brain),
    );
    let ag_target = *targets.get(&DistrictKind::Agricultural).unwrap_or(&0);
    let home = faction.home_tile();

    // Sticky-belt pre-accept. Every committed Ag plot rect that fits inside
    // the survey window joins the belt verbatim — the planner is never
    // allowed to relocate it. Footprint exclusion is computed below for
    // *new* candidates only; committed plots stay even if a later non-Ag
    // district has inflated onto them. The carve handles the per-tile
    // overlap retirement (see `carve_plots_system` + `FarmRetirements`).
    let mut parcels: Vec<Parcel> = Vec::new();
    let mut next_id = start_id;
    let mut accepted_rects: Vec<TileRect> = Vec::new();
    let mut committed_accepted: usize = 0;
    for r in committed_ag_rects {
        if accepted_rects.iter().any(|a| rects_overlap(*a, *r)) {
            continue;
        }
        let c = r.center();
        let suitability = parcel_suitability(faction, settlement, brain, chunk_map, c);
        parcels.push(Parcel {
            id: next_id,
            shape: ParcelShape::Rect(*r),
            frontage_edge: None,
            access_tile: None,
            holder: TenureHolder::State {
                faction_id: settlement.owner_faction,
            },
            district_hint: Some(DistrictKind::Agricultural),
            suitability,
        });
        accepted_rects.push(*r);
        next_id = next_id.wrapping_add(1);
        committed_accepted += 1;
    }

    let remaining_budget = budget.saturating_sub(committed_accepted);
    let remaining_target = ag_target.saturating_sub(committed_accepted);
    if remaining_target == 0 || remaining_budget == 0 {
        return parcels;
    }

    // Built-up footprint: non-ag parcels + Civic/Residential/Crafting
    // district discs, each inflated by `BELT_CLEARANCE` so fields keep a
    // breathing margin from houses/workshops.
    let inflate = |r: TileRect| -> TileRect {
        TileRect::new(
            r.x0 - BELT_CLEARANCE,
            r.y0 - BELT_CLEARANCE,
            r.w + 2 * BELT_CLEARANCE as u16,
            r.h + 2 * BELT_CLEARANCE as u16,
        )
    };
    let mut footprint: Vec<TileRect> = occupied.iter().map(|r| inflate(*r)).collect();
    for d in &brain.districts {
        if matches!(
            d.kind,
            DistrictKind::Civic | DistrictKind::Residential | DistrictKind::Crafting
        ) {
            let r = d.radius as i32;
            footprint.push(inflate(TileRect::new(
                d.centre.0 - r,
                d.centre.1 - r,
                (2 * r + 1) as u16,
                (2 * r + 1) as u16,
            )));
        }
    }
    // Committed Ag plots count as occupied territory for the *new* candidate
    // scan, so a fresh belt block cannot land on top of an existing plot.
    // They are not inflated (no clearance margin between committed Ag and a
    // proposed Ag extension — the belt is allowed to be contiguous).
    for r in committed_ag_rects {
        footprint.push(*r);
    }

    let (bw, bh) = parcel_size(DistrictKind::Agricultural, brain.phase);
    // Self-contained, home-anchored lattice — NO dependency on the fragile,
    // home-biased `best_fertile_tile` Field anchor. Blocks tile edge-to-edge
    // and stay seed-stable across re-surveys. The footprint keep-out forces
    // the belt into the first clear ring(s) OUTSIDE the built-up area; the
    // fertility ranking then steers it toward the best surrounding land.
    let ox = home.0.div_euclid(bw as i32) * bw as i32;
    let oy = home.1.div_euclid(bh as i32) * bh as i32;
    // Scan must reach beyond the footprint plus a few block rings, with a
    // sane cap so a sprawling settlement doesn't scan the whole map.
    let fp_extent = footprint
        .iter()
        .map(|r| {
            let xs = [r.x0, r.x0 + r.w as i32 - 1];
            let ys = [r.y0, r.y0 + r.h as i32 - 1];
            let mut m = 0;
            for &x in &xs {
                for &y in &ys {
                    m = m.max(cheb((x, y), home));
                }
            }
            m
        })
        .max()
        .unwrap_or(0);
    let sr = survey_radius(brain.phase);
    let scan = (fp_extent + 4 * bw as i32)
        .max(sr + 2 * bw as i32)
        .min(sr * 2 + 96);
    let span = scan / bw as i32 + 1;
    let access_radius = (bw.max(bh) as i32) / 2 + ACCESS_SOFT;

    struct AgCand {
        rect: TileRect,
        access_tile: Option<(i32, i32)>,
        suitability: ParcelSuitability,
        base_score: f32,
        tile_hash: u64,
    }
    let mut cands: Vec<AgCand> = Vec::new();
    for gy in -span..=span {
        for gx in -span..=span {
            let rect = TileRect::new(ox + gx * bw as i32, oy + gy * bh as i32, bw, bh);
            if footprint.iter().any(|f| rects_overlap(*f, rect)) {
                continue;
            }
            if !rect_clear_for_parcel(chunk_map, rect, &brain.road_corridor_tiles) {
                continue;
            }
            // 5-sample mean fertility (centre + 4 corners), mirroring
            // `compute_plot_value`'s terrain sampling. Only a `> 0` gate:
            // fertility is 0 on non-vegetated ground (Sand/Snow/Stone), so
            // this keeps fields off barren land WITHOUT an arbitrary floor
            // that would silently leave a whole region farmless.
            let c = rect.center();
            let corners = [
                c,
                (rect.x0, rect.y0),
                (rect.x0 + rect.w as i32 - 1, rect.y0),
                (rect.x0, rect.y0 + rect.h as i32 - 1),
                (rect.x0 + rect.w as i32 - 1, rect.y0 + rect.h as i32 - 1),
            ];
            let mean_fert = corners
                .iter()
                .map(|(x, y)| chunk_map.tile_fertility_at(*x, *y).unwrap_or(0) as f32 / 255.0)
                .sum::<f32>()
                / corners.len() as f32;
            if mean_fert <= 0.0 {
                continue;
            }
            // Road access is a SOFT preference, not a hard gate: a field with
            // no nearby road is still viable (a track gets carved as traffic
            // builds), so we never silently produce zero farms for lack of a
            // road. Closer-to-road blocks just score higher.
            let access_d = distance_to_road_network(chunk_map, brain, c, access_radius);
            let access_tile = access_d
                .and_then(|_| nearest_network_road_tile(chunk_map, brain, c, access_radius));
            let road_bonus = match access_d {
                Some(d) => 0.35 * (1.0 - d as f32 / access_radius as f32).clamp(0.0, 1.0),
                None => 0.0,
            };
            let suitability = parcel_suitability(faction, settlement, brain, chunk_map, c);
            // `suitability.agricultural` already = fertility*1.4 + water*0.5.
            // Soft chebyshev-distance penalty: prefer fertile near-edge sites
            // over slightly-more-fertile far-away patches. Max ~0.5 (comparable
            // to the road bonus + typical fertility deltas) so a 2× fertility
            // delta still wins, but "very slightly better and far" loses to
            // "good and adjacent".
            let near_edge_dist = (cheb(c, home) - fp_extent).max(0) as f32;
            let reach = (scan - fp_extent).max(bw as i32) as f32;
            let dist_penalty = 0.5 * (near_edge_dist / reach).min(1.0);
            let base_score = suitability.agricultural + road_bonus - dist_penalty;
            let tile_hash =
                ((c.0 as i64).wrapping_mul(0x9E37_79B9_7F4A_7C15u64 as i64) ^ c.1 as i64) as u64;
            cands.push(AgCand {
                rect,
                access_tile,
                suitability,
                base_score,
                tile_hash,
            });
        }
    }
    if cands.is_empty() {
        return Vec::new();
    }

    // Highest-fertility seed first, then grow a connected blob: each step
    // picks the remaining candidate with the most edges shared with the
    // already-accepted set (then fertility, then tile_hash) so the belt is
    // contiguous, not scattered. All tiebreaks explicit → deterministic.
    cands.sort_by(|a, b| {
        b.base_score
            .partial_cmp(&a.base_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.tile_hash.cmp(&b.tile_hash))
    });
    let block_adjacent = |a: TileRect, b: TileRect| -> bool {
        let dx = (a.x0 - b.x0).abs();
        let dy = (a.y0 - b.y0).abs();
        (dx == bw as i32 && dy == 0) || (dy == bh as i32 && dx == 0)
    };

    let cap = remaining_target.min(remaining_budget);
    let mut accepted: Vec<usize> = Vec::new();
    let mut used = vec![false; cands.len()];
    while accepted.len() < cap {
        let mut best: Option<usize> = None;
        let mut best_key: (i32, f32, u64) = (-1, f32::MIN, u64::MAX);
        for (i, c) in cands.iter().enumerate() {
            if used[i] {
                continue;
            }
            if accepted
                .iter()
                .any(|&j| rects_overlap(cands[j].rect, c.rect))
            {
                continue;
            }
            let adj = if accepted.is_empty() {
                0
            } else {
                accepted
                    .iter()
                    .filter(|&&j| block_adjacent(cands[j].rect, c.rect))
                    .count() as i32
            };
            // Rank: more shared edges (contiguity) → higher fertility →
            // lower tile_hash. First pick has `accepted` empty so adj == 0
            // for all and the highest-fertility seed wins.
            let better = best.is_none()
                || adj > best_key.0
                || (adj == best_key.0 && c.base_score > best_key.1)
                || (adj == best_key.0 && c.base_score == best_key.1 && c.tile_hash < best_key.2);
            if better {
                best = Some(i);
                best_key = (adj, c.base_score, c.tile_hash);
            }
        }
        let Some(idx) = best else { break };
        used[idx] = true;
        accepted.push(idx);
    }

    for idx in accepted {
        let c = &cands[idx];
        parcels.push(Parcel {
            id: next_id,
            shape: ParcelShape::Rect(c.rect),
            frontage_edge: None,
            access_tile: c.access_tile,
            holder: TenureHolder::State {
                faction_id: settlement.owner_faction,
            },
            district_hint: Some(DistrictKind::Agricultural),
            suitability: c.suitability.clone(),
        });
        next_id = next_id.wrapping_add(1);
    }
    parcels
}

/// Nearest `is_network_road` tile to `tile` within chebyshev `radius`, or
/// `None`. Companion to `distance_to_road_network` that returns the tile so a
/// belt parcel can record a real `access_tile` track point.
fn nearest_network_road_tile(
    chunk_map: &ChunkMap,
    brain: &SettlementBrain,
    tile: (i32, i32),
    radius: i32,
) -> Option<(i32, i32)> {
    let mut best: Option<(i32, (i32, i32))> = None;
    for dy in -radius..=radius {
        for dx in -radius..=radius {
            let d = dx.abs().max(dy.abs());
            if best.map_or(false, |(bd, _)| d >= bd) {
                continue;
            }
            let probe = (tile.0 + dx, tile.1 + dy);
            if is_network_road(chunk_map, brain, probe) {
                best = Some((d, probe));
            }
        }
    }
    best.map(|(_, t)| t)
}

/// Place a parcel rect of size `(w, h)` so that its `edge` side is adjacent
/// to the road tile (i.e. the road sits exactly one tile beyond `edge`).
fn parcel_rect_from_road(road: (i32, i32), edge: TileEdge, w: u16, h: u16) -> TileRect {
    let w_i = w as i32;
    let h_i = h as i32;
    match edge {
        // Road is north of parcel; parcel's north edge faces road.
        TileEdge::North => TileRect::new(road.0 - w_i / 2, road.1 - h_i, w, h),
        // Road is south of parcel; parcel's south edge faces road.
        TileEdge::South => TileRect::new(road.0 - w_i / 2, road.1 + 1, w, h),
        // Road is east of parcel; parcel's east edge faces road.
        TileEdge::East => TileRect::new(road.0 - w_i, road.1 - h_i / 2, w, h),
        // Road is west of parcel; parcel's west edge faces road.
        TileEdge::West => TileRect::new(road.0 + 1, road.1 - h_i / 2, w, h),
    }
}

/// Civic-milestone gating mode for `append_pressures_for_faction`.
///
/// `Runtime` runs the standard `civic_milestone_allows(Era, peak_pop)`
/// table. `Seed(maturity)` routes through `should_seed_civic` so the
/// player-chosen `StartSettlementMaturity` (Founder / Established /
/// Developed) can bypass the pop threshold at game start — matching the
/// legacy seed planner's behaviour without duplicating logic.
#[derive(Copy, Clone, Debug)]
pub enum CivicGate {
    Runtime,
    Seed(StartSettlementMaturity),
}

impl CivicGate {
    fn allows(self, kind: CivicKind, era: Era, peak_pop: u32) -> bool {
        match self {
            CivicGate::Runtime => civic_milestone_allows(kind, era, peak_pop),
            CivicGate::Seed(m) => should_seed_civic(kind, era, peak_pop, m, true),
        }
    }
}

/// Which `HearthRole` the public-hearth pressure formula counts for `era`.
/// Pre-Neolithic settled bands cluster around `Camp` hearths (no roofs);
/// Neolithic+ has indoor cooking fires and one `Civic` plaza hearth is
/// the public target. Pure function — keep in sync with
/// `desired_public_hearths`.
pub fn public_hearth_role_for_era(era: Era) -> crate::simulation::construction::HearthRole {
    match era {
        Era::Paleolithic | Era::Mesolithic => crate::simulation::construction::HearthRole::Camp,
        _ => crate::simulation::construction::HearthRole::Civic,
    }
}

/// How many public hearths a faction of `members` at `era` should have
/// near its home. Paleo/Meso `ceil(members/6)` matches the band-camp
/// crescent geometry. Neo+ is a constant `1` — extra fire comes from
/// Longhouse interiors (`HearthRole::Domestic`), which don't count here.
pub fn desired_public_hearths(era: Era, members: u32) -> u32 {
    let members = members.max(1);
    match era {
        Era::Paleolithic | Era::Mesolithic => ((members + 5) / 6).max(1),
        _ => 1,
    }
}

pub(crate) fn append_pressures_for_faction(
    _faction_id: u32,
    faction: &FactionData,
    settlement: &Settlement,
    chunk_map: &ChunkMap,
    maps: &OrganicStructureMaps,
    pending: Option<&AHashMap<BuildSiteKind, u32>>,
    civic_gate: CivicGate,
    out: &mut Vec<SettlementPressure>,
) {
    let pending_of =
        |k: BuildSiteKind| -> u32 { pending.and_then(|p| p.get(&k).copied()).unwrap_or(0) };
    let era = current_era(&faction.techs);
    let home = faction.home_tile;
    let members = faction.member_count.max(1);
    // Hearth pressure is role-aware. See `public_hearth_role_for_era` /
    // `desired_public_hearths` (below) for the contract that paleo/meso
    // count Camp against `ceil(members/6)` and Neo+ count Civic against 1.
    let counted_role = public_hearth_role_for_era(era);
    let built_hearths = maps.campfire_map.count_role_near(home, 32, counted_role) as u32;
    let desired_hearths = desired_public_hearths(era, members);
    if faction
        .techs
        .has(crate::simulation::technology::FIRE_MAKING)
        && built_hearths + pending_of(BuildSiteKind::Campfire) < desired_hearths
    {
        out.push(SettlementPressure {
            kind: SettlementPressureKind::Hearth,
            urgency: 1000.0,
            sponsor: SettlementSponsor::Chief,
            population_scope: members,
            material_budget: 1,
            reason: "public hearth",
        });
    }

    let bed_count =
        count_near(&maps.bed_map.0, home, 32) as i32 + pending_of(BuildSiteKind::Bed) as i32;
    let bed_deficit = (members as i32 - bed_count).max(0);
    if bed_deficit > 0 {
        out.push(SettlementPressure {
            kind: SettlementPressureKind::Shelter,
            urgency: 220.0 + bed_deficit as f32 * 36.0,
            sponsor: SettlementSponsor::Chief,
            population_scope: bed_deficit as u32,
            material_budget: bed_deficit as u32,
            reason: "bed deficit",
        });
    }

    if faction
        .techs
        .has(crate::simulation::technology::CROP_CULTIVATION)
        && settlement.peak_population >= 8
    {
        let food_per_head = faction.storage.food_total() / members as f32;
        if food_per_head < 18.0 {
            out.push(SettlementPressure {
                kind: SettlementPressureKind::Field,
                urgency: 120.0 + (18.0 - food_per_head) * 4.0,
                sponsor: SettlementSponsor::Chief,
                population_scope: members,
                material_budget: 0,
                reason: "food security",
            });
        }
    }

    if faction.community_has(FLINT_KNAPPING)
        && count_near(&maps.workbench_map.0, home, 28)
            + pending_of(BuildSiteKind::Workbench) as usize
            == 0
        && bed_count >= 2
    {
        out.push(SettlementPressure {
            kind: SettlementPressureKind::Craft,
            urgency: 150.0,
            sponsor: SettlementSponsor::Chief,
            population_scope: members,
            material_budget: 2,
            reason: "tool production",
        });
    }

    if faction.community_has(GRANARY)
        && civic_gate.allows(CivicKind::Granary, era, settlement.peak_population)
        && count_near(&maps.granary_map.0, home, 30) + pending_of(BuildSiteKind::Granary) as usize
            == 0
    {
        out.push(SettlementPressure {
            kind: SettlementPressureKind::Storage,
            urgency: 170.0 + faction.storage.food_total().min(200.0) * 0.1,
            sponsor: SettlementSponsor::Chief,
            population_scope: members,
            material_budget: 4,
            reason: "food storage",
        });
    }

    // Wells: gate on community adoption (mirrors Granary). Suppress the
    // first well when the home tile already abuts fresh water; second and
    // third wells emit anyway and are pushed away from the river by site
    // scoring.
    if faction.community_has(WELL_DIGGING) {
        let built_wells = count_near(&maps.well_map.0, home, 30);
        let pending_wells = pending_of(BuildSiteKind::Well) as usize;
        let target = if settlement.peak_population < 40 {
            1
        } else if settlement.peak_population < 90 {
            2
        } else {
            3
        };
        let have = built_wells + pending_wells;
        let near_fresh_water = fresh_water_within(chunk_map, home, 6);
        if have < target && !(built_wells == 0 && near_fresh_water) {
            let nearest_clean =
                nearest_fresh_or_well_distance(chunk_map, &maps.well_map.0, home, 10);
            let dist_norm = (nearest_clean as f32 / 10.0).clamp(0.0, 1.0);
            out.push(SettlementPressure {
                kind: SettlementPressureKind::WaterAccess,
                urgency: 190.0 + 60.0 * (1.0 - dist_norm),
                sponsor: SettlementSponsor::Chief,
                population_scope: members,
                material_budget: 4,
                reason: "well coverage",
            });
        }
    }

    if faction.community_has(SACRED_RITUAL)
        && civic_gate.allows(CivicKind::Shrine, era, settlement.peak_population)
        && count_near(&maps.shrine_map.0, home, 32) + pending_of(BuildSiteKind::Shrine) as usize
            == 0
    {
        out.push(SettlementPressure {
            kind: SettlementPressureKind::Ritual,
            urgency: 100.0 + (faction.culture.ceremonial as f32 / 255.0) * 170.0,
            sponsor: SettlementSponsor::Chief,
            population_scope: members,
            material_budget: 3,
            reason: "ritual focus",
        });
    }

    if faction.community_has(LONG_DIST_TRADE)
        && civic_gate.allows(CivicKind::Market, era, settlement.peak_population)
    {
        let target = if faction.culture.mercantile > 180 {
            2
        } else {
            1
        };
        if count_near(&maps.market_map.0, home, 36) + (pending_of(BuildSiteKind::Market) as usize)
            < target
        {
            out.push(SettlementPressure {
                kind: SettlementPressureKind::Trade,
                urgency: 110.0 + (faction.culture.mercantile as f32 / 255.0) * 190.0,
                sponsor: SettlementSponsor::Chief,
                population_scope: members,
                material_budget: 5,
                reason: "trade access",
            });
        }
    }

    if matches!(era, Era::Chalcolithic | Era::BronzeAge) {
        let pending_walls = pending
            .map(|p| {
                p.iter()
                    .filter_map(|(k, n)| matches!(k, BuildSiteKind::Wall(_)).then_some(*n))
                    .sum::<u32>()
            })
            .unwrap_or(0);
        let walls = count_near(&maps.wall_map.0, home, 32) as i32 + pending_walls as i32;
        let target_walls = (members as i32 * 2 + 8).min(56);
        let threat = if faction.under_raid { 2.0 } else { 1.0 };
        let deficit = (target_walls - walls).max(0);
        if deficit > 0 && bed_count > 0 {
            out.push(SettlementPressure {
                kind: SettlementPressureKind::Defense,
                urgency: (70.0 + deficit as f32 * 2.0)
                    * (0.55 + faction.culture.defensive as f32 / 255.0 * 1.45)
                    * threat,
                sponsor: SettlementSponsor::Chief,
                population_scope: members,
                material_budget: deficit as u32,
                reason: "defensible core",
            });
        }
    }

    if faction.community_has(PROFESSIONAL_ARMY)
        && civic_gate.allows(CivicKind::Barracks, era, settlement.peak_population)
        && count_near(&maps.barracks_map.0, home, 32) + pending_of(BuildSiteKind::Barracks) as usize
            == 0
    {
        out.push(SettlementPressure {
            kind: SettlementPressureKind::Military,
            urgency: 120.0 + (faction.culture.martial as f32 / 255.0) * 160.0,
            sponsor: SettlementSponsor::Chief,
            population_scope: members,
            material_budget: 6,
            reason: "organized defense",
        });
    }

    if faction.community_has(MONUMENTAL_BUILDING)
        && civic_gate.allows(CivicKind::Monument, era, settlement.peak_population)
        && count_near(&maps.monument_map.0, home, 36) + pending_of(BuildSiteKind::Monument) as usize
            == 0
    {
        out.push(SettlementPressure {
            kind: SettlementPressureKind::Monument,
            urgency: 90.0 + (faction.culture.ceremonial as f32 / 255.0) * 180.0,
            sponsor: SettlementSponsor::Chief,
            population_scope: members,
            material_budget: 10,
            reason: "prestige marker",
        });
    }

    if faction.community_has(CITY_STATE_ORG)
        && count_near(&maps.table_map.0, home, 20) + pending_of(BuildSiteKind::Table) as usize == 0
        && settlement.peak_population >= 16
    {
        out.push(SettlementPressure {
            kind: SettlementPressureKind::Governance,
            urgency: 95.0,
            sponsor: SettlementSponsor::Bureaucracy,
            population_scope: members,
            material_budget: 2,
            reason: "civic assembly",
        });
    }
}

pub(crate) fn pressure_to_intent(
    faction: &FactionData,
    brain: &SettlementBrain,
    pressure: &SettlementPressure,
    chunk_map: &ChunkMap,
    maps: &OrganicStructureMaps,
    bp_map: &BlueprintMap,
    doormat: &crate::simulation::doormat::DoormatReservations,
    archetypes: &BuildingArchetypeCatalog,
    occupied: &mut AHashSet<(i32, i32)>,
    civic_gate: CivicGate,
    road_field: &crate::simulation::placement_reachability::RoadField,
) -> Option<ConstructionIntent> {
    let seed_mode = matches!(civic_gate, CivicGate::Seed(_));
    let community_techs =
        crate::simulation::technology_adoption::community_adoption_bitset(faction);
    let era = current_era(&community_techs);
    // Step 7: era-aware wall selection. An empty `material_view` means the
    // chief-cadence classifier hasn't run yet → pass `None` (legacy
    // `best_wall_material`, no emergency). Otherwise the selector applies
    // procure-primary-first / substitute-down, and returns `EmergencyShelter`
    // when every wall rung is unobtainable (not stored, not raw-gatherable,
    // not affordably procurable) — at which point a Neolithic+ band would
    // otherwise stall shelter-less forever, so emit a bare `Bed` on the
    // era-keyed emergency annulus instead.
    let view_opt = if faction.material_view.is_empty() {
        None
    } else {
        Some(&faction.material_view)
    };
    let wall_sel = select_wall_material(&community_techs, view_opt);
    let wall_mat = wall_sel
        .mat()
        .unwrap_or_else(|| best_wall_material(&community_techs));
    // Defer post-PERM shelter one chief-classifier window when scarcity
    // hasn't been computed yet (`view_opt == None`). Without this a doomed
    // higher-tier hut is emitted on the cold-start tick, stalls forever for
    // want of material, and fills the per-faction concurrency cap — after
    // which the emergency Bed intent can never be *selected* even though it
    // is generated. One-window defer (≤ chief cadence) is the "defer with
    // reason" path; the classifier then routes to emergency or a real hut.
    if matches!(pressure.kind, SettlementPressureKind::Shelter)
        && community_techs.has(PERM_SETTLEMENT)
        && view_opt.is_none()
        && !seed_mode
    {
        return None;
    }
    let shelter_emergency = matches!(pressure.kind, SettlementPressureKind::Shelter)
        && community_techs.has(PERM_SETTLEMENT)
        && matches!(wall_sel, WallSelection::EmergencyShelter);
    let build_kind = match pressure.kind {
        SettlementPressureKind::Hearth => OrganicBuildKind::Single(BuildSiteKind::Campfire),
        SettlementPressureKind::Shelter if !community_techs.has(PERM_SETTLEMENT) => {
            OrganicBuildKind::Single(BuildSiteKind::Bed)
        }
        SettlementPressureKind::Shelter if shelter_emergency => {
            OrganicBuildKind::Single(BuildSiteKind::Bed)
        }
        SettlementPressureKind::Shelter => {
            shelter_kind(
                era,
                &community_techs,
                pressure.population_scope,
                wall_mat,
                seed_mode,
            )
        }
        SettlementPressureKind::Storage => OrganicBuildKind::Single(BuildSiteKind::Granary),
        SettlementPressureKind::Craft => OrganicBuildKind::Single(BuildSiteKind::Workbench),
        SettlementPressureKind::Ritual => OrganicBuildKind::Single(BuildSiteKind::Shrine),
        SettlementPressureKind::Trade => OrganicBuildKind::Single(BuildSiteKind::Market),
        SettlementPressureKind::Defense => OrganicBuildKind::PalisadeSegment(wall_mat),
        SettlementPressureKind::Military => OrganicBuildKind::Single(BuildSiteKind::Barracks),
        SettlementPressureKind::Monument => OrganicBuildKind::Single(BuildSiteKind::Monument),
        SettlementPressureKind::Governance => OrganicBuildKind::Single(BuildSiteKind::Table),
        SettlementPressureKind::WaterAccess => OrganicBuildKind::Single(BuildSiteKind::Well),
        SettlementPressureKind::Field => return None,
    };
    let district = district_for_pressure(pressure.kind);
    // For Hut / Longhouse the route-aware residential evaluator returns
    // (tile, picked axis-aware build_kind, door_dir). Everything else
    // routes through the legacy single-tile / shelter-emergency / palisade
    // path. Door direction defaults to the parcel's frontage for those.
    let (tile, build_kind, door_dir) = if matches!(
        build_kind,
        OrganicBuildKind::Hut(_) | OrganicBuildKind::Longhouse { .. }
    ) && !shelter_emergency
    {
        if let Some(choice) = choose_residential_site(
            faction, brain, district, build_kind, chunk_map, maps, bp_map, doormat, occupied,
            road_field,
        ) {
            (choice.tile, choice.build_kind, Some(choice.door_dir))
        } else if seed_mode {
            // Seed-mode legacy radial fallback (still returns a tile only).
            let tile = choose_site_for_intent(
                faction, brain, district, build_kind, chunk_map, maps, bp_map, doormat, occupied,
                seed_mode, road_field,
            )?;
            let dir = parcel_for_tile(brain, tile).and_then(|p| p.frontage_edge);
            (tile, build_kind, dir)
        } else {
            return None;
        }
    } else {
        let tile = if shelter_emergency {
            let bed_count = count_near(&maps.bed_map.0, faction.home_tile, 32) as i32;
            find_emergency_bed_tile(
                chunk_map,
                &maps.bed_map,
                bp_map,
                doormat,
                faction.home_tile,
                era,
                brain.layout_hash,
                bed_count,
            )
        } else if matches!(pressure.kind, SettlementPressureKind::Defense) {
            organic_palisade_site(chunk_map, maps, bp_map, doormat, brain, faction.home_tile)
        } else {
            choose_site_for_intent(
                faction, brain, district, build_kind, chunk_map, maps, bp_map, doormat, occupied,
                seed_mode, road_field,
            )
        }?;
        let dir = parcel_for_tile(brain, tile).and_then(|p| p.frontage_edge);
        (tile, build_kind, dir)
    };
    occupied.insert(tile);
    // Tag civic-pressure Hearth intents with the appropriate role so the
    // downstream blueprint/seed paths stamp the right `Campfire.role`.
    // Pre-Neolithic = `Camp` (band crescent); Neolithic+ = `Civic` (one
    // public hearth). Domestic hearths come from Longhouse interiors
    // (`walled_house_tile_plan`), not from this Hearth intent.
    let hearth_role = match pressure.kind {
        SettlementPressureKind::Hearth => Some(match era {
            Era::Paleolithic | Era::Mesolithic => {
                crate::simulation::construction::HearthRole::Camp
            }
            _ => crate::simulation::construction::HearthRole::Civic,
        }),
        _ => None,
    };
    // **Phase H** — Miasma Theory belief lift. A chief who accepts Miasma
    // Theory (`FactionData.chief_disease_belief == Some(MIASMA_THEORY)`)
    // reads foul air as the cause of sickness; sanitation infrastructure
    // (latrines, public wells) is the cure. Lift `WaterAccess` intent
    // priority by 30% so a Neolithic+ Miasma-accepting faction prioritises
    // wells / latrines earlier than a Spirit-Illness-accepting peer with
    // the same population pressure. Latrine emission isn't routed through
    // its own `SettlementPressureKind` variant yet — Phase H.3 will add a
    // dedicated `Sanitation` pressure kind. For now, the well lift is the
    // first observable behaviour difference between disease beliefs.
    let mut priority = pressure.urgency + site_bonus(brain, district, tile);
    if matches!(pressure.kind, SettlementPressureKind::WaterAccess)
        && faction.chief_disease_belief
            == Some(crate::simulation::technology::MIASMA_THEORY)
    {
        priority *= 1.30;
    }
    // **Phase H.3** — Spirit-Illness Shrine lift. A chief who accepts
    // Spirit Illness reads sickness as a ritual problem; the Shrine is
    // where the cure is sought. Lift `Ritual` intent priority by 30%
    // so a Spirit-Illness-accepting faction builds Shrines earlier than
    // a Miasma-accepting peer at the same population pressure. Mirrors
    // the Miasma → WaterAccess hook above; same magnitude keeps the two
    // belief paths symmetric in their first observable effect.
    if matches!(pressure.kind, SettlementPressureKind::Ritual)
        && faction.chief_disease_belief
            == Some(crate::simulation::technology::SPIRIT_ILLNESS)
    {
        priority *= 1.30;
    }
    Some(ConstructionIntent {
        template_id: archetype_id_for(pressure.kind, era, archetypes).to_string(),
        build_kind,
        tile,
        door_dir,
        sponsor: pressure.sponsor,
        priority,
        reason: pressure.reason,
        hearth_role,
    })
}

fn shelter_kind(
    era: Era,
    community_techs: &crate::simulation::faction::FactionTechs,
    bed_deficit: u32,
    wall_mat: WallMaterial,
    seed_mode: bool,
) -> OrganicBuildKind {
    // Phase E.2: knowledge-aware Longhouse lift. A faction that knows
    // `TIMBER_LONGHOUSE_FRAMING` has the cultural method for putting up a
    // shared-roof longhouse — drop the bed-deficit gate to `≥ 1` at
    // Neolithic+ regardless of seed-mode. Stays under the existing
    // CITY_STATE_ORG-or-Chalcolithic+ unconditional path, just opens a
    // Neolithic Longhouse lane for cultures that historically built them.
    let knows_longhouse = community_techs
        .has(crate::simulation::technology::TIMBER_LONGHOUSE_FRAMING);

    // Bootstrap P3 seed-only lift (was in the legacy `generate_candidates`):
    // at seed time, allow Longhouses at Neolithic+ once bed deficit ≥ 2.
    // The kin-group partition (settlement_bootstrap P2) puts founders into
    // ≤4-adult households, so a 6-founder Neolithic start should seed
    // visible dwelling variety (2-bed Longhouses + 1-bed Huts), not 6
    // identical huts. Runtime ladder unchanged — CITY_STATE_ORG remains the
    // post-seed gate.
    if community_techs.has(CITY_STATE_ORG)
        || (matches!(era, Era::Chalcolithic | Era::BronzeAge) && bed_deficit >= 2)
        || (seed_mode && bed_deficit >= 2)
        || (knows_longhouse
            && matches!(era, Era::Neolithic | Era::Chalcolithic | Era::BronzeAge)
            && bed_deficit >= 2)
    {
        OrganicBuildKind::Longhouse {
            wall_material: wall_mat,
            axis: HouseAxis::EastWest,
        }
    } else {
        OrganicBuildKind::Hut(wall_mat)
    }
}

// ---------------------------------------------------------------------------
// Route-aware residential scoring constants. See
// `plans/organic-residential-planner.md` for design rationale + tuning notes.
// ---------------------------------------------------------------------------

const ROUTE_BASE_BONUS: f32 = 45.0;
const ROUTE_DETOUR_PENALTY_PER_TILE: f32 = 2.0;
const ROUTE_DETOUR_RATIO_KNEE: f32 = 1.35;
const ROUTE_DETOUR_RATIO_PENALTY: f32 = 35.0;
const ROUTE_DETOUR_RATIO_LAST_RESORT: f32 = 2.75;
const ROUTE_SATURATED_PENALTY: f32 = 80.0;
/// Two `SiteChoice`s within this absolute score difference are considered
/// tied and broken lexicographically by tile coordinate for deterministic
/// placement across runs / multiplayer clients.
const ROUTE_TIE_BREAK_EPSILON: f32 = 0.5;
/// Cap on parcels evaluated per residential pressure per tick. Top-N by
/// pre-route score (`suitability × band`); keeps planner cost bounded
/// regardless of how many parcels brain has carved.
const RESIDENTIAL_PARCEL_TOP_N: usize = 32;

/// Pure-fn folding of a `PathStats` into the additive residential score
/// term. Range: roughly `[-∞, +45]`. Negative values for very long
/// detours; saturated/disconnected stats apply a flat penalty.
pub(crate) fn residential_route_score(
    stats: crate::simulation::placement_reachability::PathStats,
) -> f32 {
    let mut s = ROUTE_BASE_BONUS;
    s -= (stats.detour.max(0) as f32) * ROUTE_DETOUR_PENALTY_PER_TILE;
    let knee = stats.detour_ratio - ROUTE_DETOUR_RATIO_KNEE;
    if knee > 0.0 {
        s -= knee * ROUTE_DETOUR_RATIO_PENALTY;
    }
    if stats.saturated {
        s -= ROUTE_SATURATED_PENALTY;
    }
    s
}

/// One picked residential placement option. Carries the axis-aware
/// `build_kind` (Longhouse picks its orientation here) plus the cardinal
/// the door should face. Tile is the parcel centre; door direction maps
/// to a doormat tile via `entrance_cell_for_edge` downstream.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SiteChoice {
    pub tile: (i32, i32),
    pub build_kind: OrganicBuildKind,
    pub door_dir: TileEdge,
    pub score: f32,
    pub is_last_resort: bool,
}

/// Enumerate `(axis, cardinal)` entrance options for a residential build.
/// Returns `(build_kind_with_axis, half_w, half_h, door_dir, doormat_tile)`.
fn residential_entrance_options(
    build_kind: OrganicBuildKind,
    centre: (i32, i32),
    home: (i32, i32),
) -> Vec<(OrganicBuildKind, i32, i32, TileEdge, (i32, i32))> {
    use crate::simulation::construction::entrance_cell_for_edge;
    let cardinals = [
        TileEdge::North,
        TileEdge::East,
        TileEdge::South,
        TileEdge::West,
    ];
    let mut out = Vec::with_capacity(8);
    let axis_variants: Vec<(OrganicBuildKind, i32, i32)> = match build_kind {
        OrganicBuildKind::Hut(_) => vec![(build_kind, 1, 1)],
        OrganicBuildKind::Longhouse { wall_material, .. } => {
            vec![
                (
                    OrganicBuildKind::Longhouse {
                        wall_material,
                        axis: HouseAxis::EastWest,
                    },
                    2,
                    1,
                ),
                (
                    OrganicBuildKind::Longhouse {
                        wall_material,
                        axis: HouseAxis::NorthSouth,
                    },
                    1,
                    2,
                ),
            ]
        }
        // Other build kinds shouldn't reach this enumerator.
        _ => return Vec::new(),
    };
    for (bk, half_w, half_h) in axis_variants {
        for &edge in &cardinals {
            let entrance_off = entrance_cell_for_edge(half_w, half_h, edge, home, centre);
            let door_tile = (centre.0 + entrance_off.0, centre.1 + entrance_off.1);
            let (dx, dy) = edge.delta();
            let doormat = (door_tile.0 + dx, door_tile.1 + dy);
            out.push((bk, half_w, half_h, edge, doormat));
        }
    }
    out
}

/// Route-aware residential placement evaluator. For each parcel (top-N by
/// pre-route score), enumerates `(axis, cardinal)` options, gates on
/// existing footprint / commons / reachability checks, scores each with
/// `path_stats` from the doormat back to `home`. Returns the highest-
/// scoring `SiteChoice`; falls back to a last-resort option when every
/// candidate has `detour_ratio > ROUTE_DETOUR_RATIO_LAST_RESORT`.
#[allow(clippy::too_many_arguments)]
fn choose_residential_site(
    faction: &FactionData,
    brain: &SettlementBrain,
    district: DistrictKind,
    build_kind: OrganicBuildKind,
    chunk_map: &ChunkMap,
    maps: &OrganicStructureMaps,
    bp_map: &BlueprintMap,
    doormat_res: &crate::simulation::doormat::DoormatReservations,
    occupied: &AHashSet<(i32, i32)>,
    road_field: &crate::simulation::placement_reachability::RoadField,
) -> Option<SiteChoice> {
    use crate::simulation::placement_reachability as reach;
    let road_frontage_required = faction.community_has(PERM_SETTLEMENT);
    let commons_blocks = !build_kind_allowed_in_commons(build_kind);
    // Stage 1: cheap pre-filter — gather viable parcels with their base score
    // (suitability × band + frontage_bonus + spread), then keep top-N to
    // bound the route-eval cost.
    let mut pre: Vec<(f32, &Parcel)> = Vec::new();
    for parcel in &brain.parcels {
        let tile = parcel.centre();
        if occupied.contains(&tile) {
            continue;
        }
        if commons_blocks && tile_inside_commons(brain.commons_rect, tile) {
            continue;
        }
        if road_frontage_required
            && (parcel.frontage_edge.is_none() || parcel.access_tile.is_none())
        {
            continue;
        }
        let suitability = parcel.suitability.for_district(district);
        if suitability <= 0.05 {
            continue;
        }
        let dist = cheb(tile, faction.home_tile);
        let band = band_mul(district, brain.phase, dist);
        if band <= 0.0 {
            continue;
        }
        let frontage_bonus = parcel.frontage_edge.map(|_| 8.0).unwrap_or(0.0);
        let spread = well_spread_adjustment(build_kind, tile, chunk_map, maps);
        let base = suitability * 100.0 * band + frontage_bonus + spread;
        pre.push((base, parcel));
    }
    if pre.is_empty() {
        return None;
    }
    pre.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.centre().cmp(&b.1.centre()))
    });
    pre.truncate(RESIDENTIAL_PARCEL_TOP_N);

    let mut best: Option<SiteChoice> = None;
    let mut last_resort: Option<SiteChoice> = None;
    let home = faction.home_tile;
    let preferred_edge_for = |parcel: &Parcel| -> TileEdge {
        parcel
            .frontage_edge
            .unwrap_or_else(|| TileEdge::toward(parcel.centre(), home))
    };
    for (base_score, parcel) in pre {
        let tile = parcel.centre();
        let preferred = preferred_edge_for(parcel);
        for (bk_axis, half_w, half_h, edge, doormat) in
            residential_entrance_options(build_kind, tile, home)
        {
            // Footprint must fit per this axis.
            if !footprint_clear(tile, half_w, half_h, chunk_map, maps, bp_map, doormat_res, brain) {
                continue;
            }
            // Doormat must be a clear walkable tile (not wall / structure /
            // already-reserved doormat / road would be fine but commons
            // disc keepout still applies).
            if doormat_res.is_reserved(doormat) {
                continue;
            }
            if !chunk_map.is_passable(doormat.0, doormat.1) {
                continue;
            }
            let Some(stats) = reach::path_stats(
                chunk_map,
                road_field,
                reach::resolve3(chunk_map, doormat),
                home,
            ) else {
                continue;
            };
            let mut score = base_score + residential_route_score(stats);
            // Tiny lift for the parcel's pre-existing preferred frontage so
            // we don't churn when two cardinals tie.
            if edge == preferred {
                score += 0.25;
            }
            let is_last_resort = stats.detour_ratio > ROUTE_DETOUR_RATIO_LAST_RESORT;
            let choice = SiteChoice {
                tile,
                build_kind: bk_axis,
                door_dir: edge,
                score,
                is_last_resort,
            };
            let slot = if is_last_resort {
                &mut last_resort
            } else {
                &mut best
            };
            *slot = match *slot {
                None => Some(choice),
                Some(cur) => {
                    if score > cur.score + ROUTE_TIE_BREAK_EPSILON
                        || ((score - cur.score).abs() <= ROUTE_TIE_BREAK_EPSILON
                            && (choice.tile, choice.door_dir as u8)
                                < (cur.tile, cur.door_dir as u8))
                    {
                        Some(choice)
                    } else {
                        Some(cur)
                    }
                }
            };
        }
    }
    best.or(last_resort)
}

fn choose_site_for_intent(
    faction: &FactionData,
    brain: &SettlementBrain,
    district: DistrictKind,
    build_kind: OrganicBuildKind,
    chunk_map: &ChunkMap,
    maps: &OrganicStructureMaps,
    bp_map: &BlueprintMap,
    doormat: &crate::simulation::doormat::DoormatReservations,
    occupied: &AHashSet<(i32, i32)>,
    seed_mode: bool,
    road_field: &crate::simulation::placement_reachability::RoadField,
) -> Option<(i32, i32)> {
    let mut candidates: Vec<(f32, (i32, i32))> = Vec::new();
    let road_frontage_required =
        faction.community_has(PERM_SETTLEMENT) && build_kind_requires_frontage(build_kind);
    let commons_blocks = !build_kind_allowed_in_commons(build_kind);
    for parcel in &brain.parcels {
        let tile = parcel.centre();
        if occupied.contains(&tile) {
            continue;
        }
        // L1: civic commons keepout — non-civic builds may not anchor inside
        // the commons disc.
        if commons_blocks && tile_inside_commons(brain.commons_rect, tile) {
            continue;
        }
        if road_frontage_required
            && (parcel.frontage_edge.is_none() || parcel.access_tile.is_none())
        {
            continue;
        }
        let suitability = parcel.suitability.for_district(district);
        if suitability <= 0.05 {
            continue;
        }
        if !intent_site_clear(build_kind, tile, chunk_map, maps, bp_map, doormat, brain) {
            continue;
        }
        // Runtime reachability: the organic planner doesn't validate that a
        // parcel is walkable from home (it can sit across a river). Reject
        // parcels a worker could never path to.
        if !crate::simulation::placement_reachability::tile_reachable_from_home(
            chunk_map,
            faction.home_tile,
            tile,
        ) {
            continue;
        }
        let frontage_bonus = parcel.frontage_edge.map(|_| 8.0).unwrap_or(0.0);
        let spread = well_spread_adjustment(build_kind, tile, chunk_map, maps);
        // L4: distance-band scoring. Outside `[min, max]` the band is 0 and
        // the parcel hard-rejects; inside, the triangular curve weights the
        // suitability score so a mid-band tile beats a center-clinging tile.
        let dist = cheb(tile, faction.home_tile);
        let band = band_mul(district, brain.phase, dist);
        if band <= 0.0 {
            continue;
        }
        candidates.push((suitability * 100.0 * band + frontage_bonus + spread, tile));
    }
    for &tile in &brain.frontier {
        if road_frontage_required {
            continue;
        }
        if occupied.contains(&tile) {
            continue;
        }
        if commons_blocks && tile_inside_commons(brain.commons_rect, tile) {
            continue;
        }
        if !intent_site_clear(build_kind, tile, chunk_map, maps, bp_map, doormat, brain) {
            continue;
        }
        if !crate::simulation::placement_reachability::tile_reachable_from_home(
            chunk_map,
            faction.home_tile,
            tile,
        ) {
            continue;
        }
        let spread = well_spread_adjustment(build_kind, tile, chunk_map, maps);
        let dist = cheb(tile, faction.home_tile);
        let band = band_mul(district, brain.phase, dist);
        if band <= 0.0 {
            continue;
        }
        candidates.push((site_bonus(brain, district, tile) * band + spread, tile));
    }
    candidates.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.cmp(&b.1))
    });
    if let Some((_, tile)) = candidates.first() {
        return Some(*tile);
    }

    // Home-radial fallback — **seed mode only**. At runtime the chief
    // genuinely should stall when no parcel-fronted lot fits; the next
    // morphology tick reruns the survey and gives it real parcels with
    // frontage. At seed time the brain may have 0–1 parcels for a
    // freshly-surveyed faction (small survey window, sparse road
    // anchors), so without this fallback Shelter / Hearth / civic
    // intents starve and the seed pipeline produces no beds. The
    // fallback honours every gate (commons / distance band /
    // intent_site_clear / reachability), it only relaxes the
    // parcel-frontage requirement.
    if !seed_mode {
        return None;
    }
    home_radial_fallback(
        faction,
        brain,
        district,
        build_kind,
        chunk_map,
        maps,
        bp_map,
        doormat,
        occupied,
        commons_blocks,
        road_frontage_required,
        road_field,
    )
}

/// Phase-keyed spiral scan around `home_tile` that finds the highest-band
/// tile passing every `choose_site_for_intent` gate. Reserved for the
/// cold-start case where the brain hasn't yet generated parcels with
/// matching frontage. Bounded by `survey_radius(phase) + 4` so it stays
/// inside loaded terrain.
#[allow(clippy::too_many_arguments)]
fn home_radial_fallback(
    faction: &FactionData,
    brain: &SettlementBrain,
    district: DistrictKind,
    build_kind: OrganicBuildKind,
    chunk_map: &ChunkMap,
    maps: &OrganicStructureMaps,
    bp_map: &BlueprintMap,
    doormat: &crate::simulation::doormat::DoormatReservations,
    occupied: &AHashSet<(i32, i32)>,
    commons_blocks: bool,
    road_frontage_required: bool,
    road_field: &crate::simulation::placement_reachability::RoadField,
) -> Option<(i32, i32)> {
    // Frontage-required intents (Hut / Longhouse / CompositeHouse) still
    // run the radial fallback when no road-fronted parcel was found.
    // `seed_walled_house_at` resolves the door direction relative to home
    // when `door_dir == None`, and the doormat tile pushes onto
    // `RoadCarveQueue` so the next survey re-rolls the parcel network with
    // real frontage. Suppressing the fallback for frontage builds would
    // starve every Neolithic+ seeded village whose brain hasn't ratcheted
    // up parcels yet.
    let _ = road_frontage_required;
    let max_r = survey_radius(brain.phase).max(8) + 4;
    let home = faction.home_tile;
    let (half_w, half_h) = match build_kind {
        OrganicBuildKind::Hut(_) => (1, 1),
        OrganicBuildKind::Longhouse { axis, .. } => axis.longhouse_halves(),
        // Composite footprints aren't emitted by the organic pressure
        // path (composite shelter is disabled in `pressure_to_intent` —
        // the 2×2+2×1 mask has no interior bed cells). Use the longhouse
        // bounding box as a conservative default if one ever appears so
        // the radial fallback still rejects commons-overlap.
        OrganicBuildKind::CompositeHouse { .. } => (2, 1),
        OrganicBuildKind::Single(_) | OrganicBuildKind::PalisadeSegment(_) => (0, 0),
    };
    use crate::simulation::placement_reachability as reach;
    let mut best: Option<(f32, (i32, i32))> = None;
    // Don't early-return on first ring hit. Look `max_r/2` further rings so
    // a clearly cleaner outer candidate can beat a barely-passing inner one.
    let mut first_hit_ring: Option<i32> = None;
    let extra_rings = (max_r / 2).max(1);
    for r in 1..=max_r {
        if let Some(hit) = first_hit_ring {
            if r > hit + extra_rings {
                break;
            }
        }
        for dy in -r..=r {
            for dx in -r..=r {
                if dx.abs().max(dy.abs()) != r {
                    continue;
                }
                let tile = (home.0 + dx, home.1 + dy);
                if occupied.contains(&tile) {
                    continue;
                }
                if commons_blocks {
                    let foot = TileRect::new(
                        tile.0 - half_w,
                        tile.1 - half_h,
                        (2 * half_w + 1) as u16,
                        (2 * half_h + 1) as u16,
                    );
                    if rect_intersects_commons(brain.commons_rect, foot) {
                        continue;
                    }
                }
                let band = band_mul(district, brain.phase, r);
                if band <= 0.0 {
                    continue;
                }
                if !intent_site_clear(build_kind, tile, chunk_map, maps, bp_map, doormat, brain) {
                    continue;
                }
                if !reach::tile_reachable_from_home(chunk_map, home, tile) {
                    continue;
                }
                let spread = well_spread_adjustment(build_kind, tile, chunk_map, maps);
                let mut score = band * 100.0 + spread;
                // Route-aware lift: a candidate one ring further out with a
                // direct walk to home should beat a closer candidate whose
                // walked route loops around.
                if let Some(stats) = reach::path_stats(
                    chunk_map,
                    road_field,
                    reach::resolve3(chunk_map, tile),
                    home,
                ) {
                    score += residential_route_score(stats);
                }
                if best.map_or(true, |b| score > b.0) {
                    best = Some((score, tile));
                    if first_hit_ring.is_none() {
                        first_hit_ring = Some(r);
                    }
                }
            }
        }
    }
    best.map(|(_, tile)| tile)
}

/// Per-well penalty that pushes 2nd/3rd wells away from existing wells and
/// from rivers/bridges. Zero for non-Well intents. Penalty falls off
/// linearly to zero past 10 chebyshev tiles.
fn well_spread_adjustment(
    build_kind: OrganicBuildKind,
    tile: (i32, i32),
    chunk_map: &ChunkMap,
    maps: &OrganicStructureMaps,
) -> f32 {
    if !matches!(build_kind, OrganicBuildKind::Single(BuildSiteKind::Well)) {
        return 0.0;
    }
    let mut penalty = 0.0;
    for &well_tile in maps.well_map.0.keys() {
        let d = cheb(tile, well_tile);
        if d < 10 {
            penalty -= (10 - d) as f32 * 8.0;
        }
    }
    for r in 1i32..=10 {
        for dy in -r..=r {
            for dx in -r..=r {
                if dx.abs().max(dy.abs()) != r {
                    continue;
                }
                let probe = (tile.0 + dx, tile.1 + dy);
                if let Some(kind) = chunk_map.tile_kind_at(probe.0, probe.1) {
                    if matches!(kind, TileKind::River | TileKind::Bridge) {
                        penalty -= (10 - r) as f32 * 4.0;
                        // Only count the single nearest fresh-water tile
                        // (rings expand outward, so the first hit wins).
                        return penalty;
                    }
                }
            }
        }
    }
    penalty
}

fn build_kind_requires_frontage(build_kind: OrganicBuildKind) -> bool {
    matches!(
        build_kind,
        OrganicBuildKind::Hut(_)
            | OrganicBuildKind::Longhouse { .. }
            | OrganicBuildKind::CompositeHouse { .. }
    )
}

fn intent_site_clear(
    build_kind: OrganicBuildKind,
    tile: (i32, i32),
    chunk_map: &ChunkMap,
    maps: &OrganicStructureMaps,
    bp_map: &BlueprintMap,
    doormat: &crate::simulation::doormat::DoormatReservations,
    brain: &SettlementBrain,
) -> bool {
    match build_kind {
        // Wells carry a 5×5 stepwell footprint, not a single tile. Route the
        // whole disc through the shared `well_footprint_clear` predicate so
        // the organic placement lane agrees with the seed-time search and
        // runtime blueprint converter on what "buildable" means.
        OrganicBuildKind::Single(BuildSiteKind::Well) => {
            // Wells are stamped with an outer-ring lining wall (minus the
            // gateway) and `road_carve_system` consults `WellMap` to route
            // carved roads around the 5×5 disc, so a planned spine
            // overlapping the future footprint is no longer fatal — the
            // walls land first and the carver detours. Pass `brain: None`
            // to skip the `road_tiles` / `road_corridor_tiles` check; the
            // chunk-map `Road`/`Wall` gate inside `well_footprint_clear`
            // still rejects tiles already paved or walled.
            let empty_well_site_map =
                crate::simulation::well::WellSiteMap::default();
            let ctx = crate::simulation::well::WellPlacementCtx {
                structure_index: maps.structure_index,
                bp_map,
                well_map: maps.well_map,
                well_site_map: &empty_well_site_map,
                doormat: Some(doormat),
                seed_reservation: None,
                brain: None,
                chunk_map: Some(chunk_map),
                used: None,
                self_bp: None,
            };
            crate::simulation::well::well_footprint_clear(tile, &ctx).is_ok()
        }
        OrganicBuildKind::Single(_) => {
            single_tile_clear(tile, chunk_map, maps, bp_map, doormat, brain)
        }
        OrganicBuildKind::Hut(_) => {
            footprint_clear(tile, 1, 1, chunk_map, maps, bp_map, doormat, brain)
        }
        OrganicBuildKind::Longhouse { axis, .. } => {
            let (hw, hh) = axis.longhouse_halves();
            footprint_clear(tile, hw, hh, chunk_map, maps, bp_map, doormat, brain)
        }
        OrganicBuildKind::PalisadeSegment(_) => {
            single_tile_clear(tile, chunk_map, maps, bp_map, doormat, brain)
        }
        OrganicBuildKind::CompositeHouse {
            shape, rotation, ..
        } => {
            for tile in crate::simulation::building_template::shape_tiles(shape, tile, rotation) {
                if !single_tile_clear(tile, chunk_map, maps, bp_map, doormat, brain) {
                    return false;
                }
            }
            true
        }
    }
}

fn single_tile_clear(
    tile: (i32, i32),
    chunk_map: &ChunkMap,
    maps: &OrganicStructureMaps,
    bp_map: &BlueprintMap,
    doormat: &crate::simulation::doormat::DoormatReservations,
    brain: &SettlementBrain,
) -> bool {
    if bp_map.0.contains_key(&tile)
        || maps.structure_index.0.contains_key(&tile)
        || maps.bed_map.0.contains_key(&tile)
        // A well's 5×5 footprint must reject non-well placements landing on
        // any of its tiles — including the central shaft, which is registered
        // in `WellMap` but not `StructureIndex` (the wellhead carries its own
        // `StructureLabel`, but the eight inner-ring helix tiles do not).
        || maps.well_map.0.contains_key(&tile)
        || doormat.is_reserved(tile)
        || brain.road_tiles.contains(&tile)
        // The widened corridor includes the perpendicular tile next to each
        // spine cell — what `road_carve_system` actually paints. Parcel
        // allocation already rejects against this; if a footprint enumerator
        // (residential evaluator or seed radial fallback) anchors inside or
        // straddles a corridor tile, the carver will later paint Road on
        // top of the wall and break pathfinding through the dwelling.
        || brain.road_corridor_tiles.contains(&tile)
    {
        return false;
    }
    let Some(kind) = chunk_map.tile_kind_at(tile.0, tile.1) else {
        return false;
    };
    kind.is_passable() && kind != TileKind::Wall && kind != TileKind::Road
}

fn footprint_clear(
    centre: (i32, i32),
    half_w: i32,
    half_h: i32,
    chunk_map: &ChunkMap,
    maps: &OrganicStructureMaps,
    bp_map: &BlueprintMap,
    doormat: &crate::simulation::doormat::DoormatReservations,
    brain: &SettlementBrain,
) -> bool {
    for dy in -half_h..=half_h {
        for dx in -half_w..=half_w {
            if !single_tile_clear(
                (centre.0 + dx, centre.1 + dy),
                chunk_map,
                maps,
                bp_map,
                doormat,
                brain,
            ) {
                return false;
            }
        }
    }
    true
}

fn organic_palisade_site(
    chunk_map: &ChunkMap,
    maps: &OrganicStructureMaps,
    bp_map: &BlueprintMap,
    doormat: &crate::simulation::doormat::DoormatReservations,
    brain: &SettlementBrain,
    home: (i32, i32),
) -> Option<(i32, i32)> {
    let mut min_x = i32::MAX;
    let mut max_x = i32::MIN;
    let mut min_y = i32::MAX;
    let mut max_y = i32::MIN;
    for &tile in maps.bed_map.0.keys() {
        if cheb(tile, home) > 32 {
            continue;
        }
        min_x = min_x.min(tile.0);
        max_x = max_x.max(tile.0);
        min_y = min_y.min(tile.1);
        max_y = max_y.max(tile.1);
    }
    if min_x == i32::MAX {
        return None;
    }
    let buffer = 3;
    min_x -= buffer;
    max_x += buffer;
    min_y -= buffer;
    max_y += buffer;
    let gateway_half = 1;

    for x in min_x..=max_x {
        for y in [min_y, max_y] {
            if (x - home.0).abs() <= gateway_half {
                continue;
            }
            let tile = (x, y);
            if single_tile_clear(tile, chunk_map, maps, bp_map, doormat, brain) {
                return Some(tile);
            }
        }
    }
    for y in min_y + 1..max_y {
        for x in [min_x, max_x] {
            if (y - home.1).abs() <= gateway_half {
                continue;
            }
            let tile = (x, y);
            if single_tile_clear(tile, chunk_map, maps, bp_map, doormat, brain) {
                return Some(tile);
            }
        }
    }
    None
}

fn archetype_id_for(
    pressure: SettlementPressureKind,
    era: Era,
    archetypes: &BuildingArchetypeCatalog,
) -> &'static str {
    let wanted = match pressure {
        SettlementPressureKind::Hearth => "hearth_cluster",
        SettlementPressureKind::Shelter => "household_dwelling",
        SettlementPressureKind::Field => "field_edge",
        SettlementPressureKind::Storage => "granary",
        SettlementPressureKind::Craft => "workshop",
        SettlementPressureKind::Ritual => "shrine",
        SettlementPressureKind::Trade => "market",
        SettlementPressureKind::Defense => "palisade",
        SettlementPressureKind::Military => "barracks",
        SettlementPressureKind::Monument => "monument",
        SettlementPressureKind::Governance => "assembly",
        SettlementPressureKind::WaterAccess => "well",
    };
    if archetypes.for_era(era).any(|a| a.id == wanted) {
        wanted
    } else {
        "fallback"
    }
}

fn district_for_pressure(kind: SettlementPressureKind) -> DistrictKind {
    match kind {
        SettlementPressureKind::Hearth | SettlementPressureKind::Governance => DistrictKind::Civic,
        SettlementPressureKind::Shelter => DistrictKind::Residential,
        SettlementPressureKind::Field => DistrictKind::Agricultural,
        SettlementPressureKind::Storage => DistrictKind::Storage,
        SettlementPressureKind::Craft => DistrictKind::Crafting,
        SettlementPressureKind::Ritual | SettlementPressureKind::Monument => DistrictKind::Sacred,
        SettlementPressureKind::Trade => DistrictKind::Market,
        SettlementPressureKind::Defense | SettlementPressureKind::Military => DistrictKind::Defense,
        SettlementPressureKind::WaterAccess => DistrictKind::Civic,
    }
}

fn site_bonus(brain: &SettlementBrain, district: DistrictKind, tile: (i32, i32)) -> f32 {
    let mut score = 0.0;
    for d in &brain.districts {
        if d.kind != district {
            continue;
        }
        let dist = cheb(tile, d.centre) as f32;
        let radius = d.radius.max(1) as f32;
        if dist <= radius {
            score += (1.0 - dist / radius) * d.weight * 50.0;
        }
    }
    score += brain.traffic_heat.get(&tile).copied().unwrap_or(0) as f32
        * if matches!(district, DistrictKind::Market | DistrictKind::Civic) {
            0.35
        } else {
            0.08
        };
    score
}

fn material_scarcity_penalty(build_kind: OrganicBuildKind, faction: &FactionData) -> f32 {
    let mut penalty = 0.0;
    for (rid, qty) in required_goods(build_kind) {
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
    penalty
}

fn required_goods(
    build_kind: OrganicBuildKind,
) -> AHashMap<crate::economy::resource_catalog::ResourceId, u32> {
    let mut totals = AHashMap::default();
    let mut add = |kind: BuildSiteKind, n: u32| {
        for &(rid, qty) in &recipe_for(kind).inputs {
            *totals.entry(rid).or_insert(0) += qty as u32 * n;
        }
    };
    match build_kind {
        OrganicBuildKind::Single(kind) => add(kind, 1),
        OrganicBuildKind::Hut(mat) => {
            add(BuildSiteKind::Wall(mat), 4);
            add(BuildSiteKind::Door, 1);
            add(BuildSiteKind::Bed, 1);
        }
        OrganicBuildKind::Longhouse { wall_material, .. } => {
            add(BuildSiteKind::Wall(wall_material), 8);
            add(BuildSiteKind::Door, 1);
            add(BuildSiteKind::Bed, 2);
        }
        OrganicBuildKind::PalisadeSegment(mat) => add(BuildSiteKind::Wall(mat), 1),
        OrganicBuildKind::CompositeHouse {
            shape,
            rotation,
            wall_material,
        } => {
            let tiles = crate::simulation::building_template::shape_tiles(shape, (0, 0), rotation);
            let tile_set: AHashSet<(i32, i32)> = tiles.iter().copied().collect();
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

fn intent_tech_allowed(build_kind: OrganicBuildKind, faction: &FactionData) -> bool {
    let techs = crate::simulation::technology_adoption::community_adoption_bitset(faction);
    match build_kind {
        OrganicBuildKind::Single(kind) => faction_can_build(kind, &techs),
        OrganicBuildKind::Hut(mat) => {
            faction_can_build(BuildSiteKind::Wall(mat), &techs)
                && faction_can_build(BuildSiteKind::Door, &techs)
                && faction_can_build(BuildSiteKind::Bed, &techs)
        }
        OrganicBuildKind::Longhouse { wall_material, .. } => {
            faction_can_build(BuildSiteKind::Wall(wall_material), &techs)
                && faction_can_build(BuildSiteKind::Door, &techs)
                && faction_can_build(BuildSiteKind::Bed, &techs)
        }
        OrganicBuildKind::PalisadeSegment(mat) => {
            faction_can_build(BuildSiteKind::Wall(mat), &techs)
        }
        OrganicBuildKind::CompositeHouse { wall_material, .. } => {
            faction_can_build(BuildSiteKind::Wall(wall_material), &techs)
                && faction_can_build(BuildSiteKind::Door, &techs)
                && faction_can_build(BuildSiteKind::Bed, &techs)
        }
    }
}

fn pending_kind_counts(
    bp_map: &BlueprintMap,
    bp_query: &Query<&Blueprint>,
) -> AHashMap<u32, AHashMap<BuildSiteKind, u32>> {
    let mut pending = AHashMap::default();
    for &entity in bp_map.0.values() {
        let Ok(bp) = bp_query.get(entity) else {
            continue;
        };
        *pending
            .entry(bp.faction_id)
            .or_insert_with(AHashMap::default)
            .entry(bp.kind)
            .or_insert(0) += 1;
    }
    pending
}

fn blueprint_counts_by_faction(
    bp_map: &BlueprintMap,
    bp_query: &Query<&Blueprint>,
    pending_footprints: &PendingFootprints,
) -> AHashMap<u32, usize> {
    let mut out = AHashMap::default();
    for &entity in bp_map.0.values() {
        if let Ok(bp) = bp_query.get(entity) {
            *out.entry(bp.faction_id).or_insert(0) += 1;
        }
    }
    for pending in &pending_footprints.queue {
        *out.entry(pending.faction_id).or_insert(0) += 1;
    }
    out
}

fn parcel_for_tile(brain: &SettlementBrain, tile: (i32, i32)) -> Option<&Parcel> {
    brain.parcels.iter().find(|p| {
        let rect = p.rect();
        rect.contains(tile.0, tile.1)
    })
}

fn tile_open_for_frontier(
    chunk_map: &ChunkMap,
    maps: &SurveyStructureSnapshot,
    tile: (i32, i32),
) -> bool {
    if maps.structures.contains(&tile) {
        return false;
    }
    let Some(kind) = chunk_map.tile_kind_at(tile.0, tile.1) else {
        return false;
    };
    kind.is_passable() && kind != TileKind::Wall && !kind.is_water_like()
}

fn frontier_score<F: SurveyFactionView>(
    faction: &F,
    brain: &SettlementBrain,
    chunk_map: &ChunkMap,
    tile: (i32, i32),
) -> f32 {
    let fertility = chunk_map.tile_fertility_at(tile.0, tile.1).unwrap_or(0) as f32 / 255.0;
    let river_d = chunk_map.river_distance_at(tile.0, tile.1);
    let water = if river_d <= 5 {
        1.0 - river_d as f32 / 8.0
    } else {
        0.0
    };
    let slope_penalty = local_slope(chunk_map, tile) as f32 * 0.25;
    let heat = brain.traffic_heat.get(&tile).copied().unwrap_or(0) as f32 / 255.0;
    let centre_d = cheb(tile, faction.home_tile()) as f32;
    let density_pull = faction.culture().density as f32 / 255.0;
    let spacing = if centre_d < 4.0 {
        -2.0
    } else {
        (1.0 / (1.0 + centre_d * if density_pull > 0.5 { 0.08 } else { 0.03 })) * 2.0
    };
    1.0 + fertility * 2.0 + water * 1.5 + heat * 2.0 + spacing - slope_penalty
}

fn parcel_suitability<F: SurveyFactionView>(
    faction: &F,
    settlement: &Settlement,
    brain: &SettlementBrain,
    chunk_map: &ChunkMap,
    tile: (i32, i32),
) -> ParcelSuitability {
    let fertility = chunk_map.tile_fertility_at(tile.0, tile.1).unwrap_or(0) as f32 / 255.0;
    let river_d = chunk_map.river_distance_at(tile.0, tile.1);
    let water_bonus = if river_d <= 5 {
        1.0 - river_d as f32 / 6.0
    } else {
        0.0
    };
    let home = faction.home_tile();
    let home_d = cheb(tile, home) as f32;
    let heat = brain.traffic_heat.get(&tile).copied().unwrap_or(0) as f32 / 255.0;
    // Terrain elevation delta (solid ground, not water surface).
    let high = (chunk_map.ground_z_at(tile.0, tile.1) - chunk_map.ground_z_at(home.0, home.1))
        .max(0) as f32;
    let mut s = ParcelSuitability {
        residential: 0.65 + water_bonus * 0.25 + (1.0 / (1.0 + home_d * 0.08)) * 0.5,
        agricultural: fertility * 1.4 + water_bonus * 0.5,
        crafting: 0.35 + heat * 0.35 + material_anchor_bonus(brain, tile) * 0.4,
        civic: 0.55 + (1.0 / (1.0 + home_d * 0.12)) * 0.8 + heat * 0.2,
        defense: high * 0.08 + (home_d / survey_radius(brain.phase) as f32).clamp(0.0, 1.0),
        storage: 0.45 + fertility * 0.25 + (1.0 / (1.0 + home_d * 0.07)) * 0.5,
        sacred: 0.35
            + high * 0.06
            + (faction.culture().ceremonial as f32 / 255.0) * 0.35
            + (1.0 / (1.0 + home_d * 0.05)) * 0.2,
        market: 0.3 + heat * 0.9 + (faction.culture().mercantile as f32 / 255.0) * 0.3,
    };
    if settlement.peak_population < 10 {
        s.market *= 0.25;
        s.defense *= 0.35;
        s.sacred *= 0.5;
    }
    s
}

fn best_district_for_parcel(s: &ParcelSuitability) -> Option<DistrictKind> {
    let candidates = [
        (DistrictKind::Residential, s.residential),
        (DistrictKind::Agricultural, s.agricultural),
        (DistrictKind::Crafting, s.crafting),
        (DistrictKind::Civic, s.civic),
        (DistrictKind::Defense, s.defense),
        (DistrictKind::Storage, s.storage),
        (DistrictKind::Sacred, s.sacred),
        (DistrictKind::Market, s.market),
    ];
    candidates
        .into_iter()
        .filter(|(_, score)| *score > 0.4)
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(kind, _)| kind)
}

/// Total tile count of every Agricultural parcel currently on `brain`. Used
/// by `parcel_targets` as the seed-budget floor so a faction running an
/// active belt doesn't shrink-plan itself below productive size on a
/// transient low-seed reading.
fn current_ag_tile_count(brain: &SettlementBrain) -> u32 {
    brain
        .parcels
        .iter()
        .filter(|p| matches!(p.district_hint, Some(DistrictKind::Agricultural)))
        .map(|p| match p.shape {
            ParcelShape::Rect(r) => (r.w as u32).saturating_mul(r.h as u32),
            _ => 0,
        })
        .sum()
}

// Annual-planning constants now live in `farm.rs` (single source of truth,
// shared with the seasonal `farm_pressure` signal). Re-exported here so
// `parcel_targets` and other call sites don't churn.
pub use crate::simulation::farm::{
    GRAIN_PER_PERSON_PER_YEAR, GRAIN_YIELD_PER_TILE_PLANNING, SUPPLY_SAFETY_DENOM,
    SUPPLY_SAFETY_NUMER,
};

fn parcel_targets<F: SurveyFactionView>(
    faction: &F,
    settlement: &Settlement,
    phase: SettlementPhase,
    current_ag_tiles: u32,
) -> AHashMap<DistrictKind, usize> {
    let members = faction
        .member_count()
        .max(settlement.peak_population)
        .max(1) as usize;
    let techs = faction.techs();
    let era = current_era(&techs);
    let mut targets = AHashMap::default();

    targets.insert(DistrictKind::Civic, 1);
    targets.insert(DistrictKind::Residential, ((members + 3) / 4).clamp(2, 24));

    if faction.community_has(CROP_CULTIVATION) {
        // Seasonal-farming jellyfish: demand-driven plot count based on
        // (food need ∩ labor cap ∩ seed stock), divided by the average
        // active-tiles-per-plot (~96, plot is 16×16 = 256 tiles, ~3/8 of
        // which is plantable Cropland in the new mosaic).
        // food_tiles = members × annual grain need × safety ÷ per-tile yield.
        // ≈ members × 15 (48 × 1.25 ÷ 4) ⇒ a 20-tribe needs ~300 tiles.
        let food_tiles = ((members as u32) * GRAIN_PER_PERSON_PER_YEAR * SUPPLY_SAFETY_NUMER)
            .div_ceil(SUPPLY_SAFETY_DENOM * GRAIN_YIELD_PER_TILE_PLANNING);
        let labor_tiles = (((members as u32) * 60) / 100).saturating_mul(24);
        let grain_seed_stock = faction.seed_total();
        // Don't let the seed budget shrink below tiles already in production
        // — each grain harvest yields its seed cofactor, so an active field
        // is self-sustaining once it ran its first cycle. The 32-tile floor
        // covers the first-season case where the band has no harvest yet.
        let seed_tiles = grain_seed_stock.max(current_ag_tiles).max(32);
        let target_active = food_tiles.min(labor_tiles).min(seed_tiles);
        let target_plots = (((target_active + 95) / 96).max(1)).min(12) as usize;
        targets.insert(DistrictKind::Agricultural, target_plots);
    }
    if faction.community_has(FLINT_KNAPPING) {
        targets.insert(DistrictKind::Crafting, 2);
    }
    if faction.community_has(PERM_SETTLEMENT) || faction.community_has(GRANARY) {
        targets.insert(DistrictKind::Storage, 2);
    }
    if faction.community_has(SACRED_RITUAL) {
        targets.insert(DistrictKind::Sacred, 2);
    }
    if faction.community_has(LONG_DIST_TRADE) {
        let market_target = if faction.culture().mercantile > 180 {
            2
        } else {
            1
        };
        targets.insert(DistrictKind::Market, market_target);
    }
    if matches!(era, Era::Chalcolithic | Era::BronzeAge)
        || matches!(
            phase,
            SettlementPhase::Chiefdom | SettlementPhase::ProtoUrban | SettlementPhase::Urban
        )
    {
        targets.insert(DistrictKind::Defense, 8);
    }

    targets
}

fn choose_district_for_parcel(
    s: &ParcelSuitability,
    targets: &AHashMap<DistrictKind, usize>,
    counts: &AHashMap<DistrictKind, usize>,
) -> Option<DistrictKind> {
    let candidates = [
        (DistrictKind::Residential, s.residential),
        (DistrictKind::Agricultural, s.agricultural),
        (DistrictKind::Crafting, s.crafting),
        (DistrictKind::Civic, s.civic),
        (DistrictKind::Defense, s.defense),
        (DistrictKind::Storage, s.storage),
        (DistrictKind::Sacred, s.sacred),
        (DistrictKind::Market, s.market),
    ];
    candidates
        .into_iter()
        .filter_map(|(kind, score)| {
            let target = targets.get(&kind).copied().unwrap_or(0);
            if counts.get(&kind).copied().unwrap_or(0) >= target {
                return None;
            }
            let threshold = match kind {
                DistrictKind::Residential | DistrictKind::Civic => 0.25,
                DistrictKind::Agricultural => 0.35,
                _ => 0.4,
            };
            (score > threshold).then_some((kind, score))
        })
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(kind, _)| kind)
}

fn parcel_size(kind: DistrictKind, phase: SettlementPhase) -> (u16, u16) {
    match kind {
        DistrictKind::Residential => {
            if matches!(phase, SettlementPhase::ProtoUrban | SettlementPhase::Urban) {
                (6, 5)
            } else {
                (5, 5)
            }
        }
        DistrictKind::Agricultural => (16, 16),
        DistrictKind::Crafting | DistrictKind::Storage | DistrictKind::Market => (5, 4),
        DistrictKind::Civic | DistrictKind::Sacred => (5, 5),
        DistrictKind::Defense => (3, 3),
    }
}

fn rect_clear_for_parcel(
    chunk_map: &ChunkMap,
    rect: TileRect,
    protected_roads: &AHashSet<(i32, i32)>,
) -> bool {
    for y in rect.y0..rect.y0 + rect.h as i32 {
        for x in rect.x0..rect.x0 + rect.w as i32 {
            if protected_roads.contains(&(x, y)) {
                return false;
            }
            let Some(kind) = chunk_map.tile_kind_at(x, y) else {
                return false;
            };
            if !kind.is_passable()
                || kind.is_water_like()
                || kind == TileKind::Wall
                || kind == TileKind::Road
            {
                return false;
            }
        }
    }
    true
}

fn frontage_to_network(
    chunk_map: &ChunkMap,
    brain: &SettlementBrain,
    rect: TileRect,
) -> (Option<TileEdge>, Option<(i32, i32)>) {
    let cx = rect.x0 + rect.w as i32 / 2;
    let cy = rect.y0 + rect.h as i32 / 2;
    let probes = [
        (TileEdge::North, (cx, rect.y0 + rect.h as i32), (0, 1)),
        (TileEdge::South, (cx, rect.y0 - 1), (0, -1)),
        (TileEdge::East, (rect.x0 + rect.w as i32, cy), (1, 0)),
        (TileEdge::West, (rect.x0 - 1, cy), (-1, 0)),
    ];
    let mut best: Option<(i32, TileEdge, (i32, i32))> = None;
    for (edge, start, dir) in probes {
        // Organic lots require literal frontage: the road tile must sit just
        // outside the parcel edge. A road merely "nearby" can leave the door's
        // doormat open but disconnected by a one-tile gap.
        for step in 0..=0 {
            let tile = (start.0 + dir.0 * step, start.1 + dir.1 * step);
            if is_network_road(chunk_map, brain, tile) {
                if best.map(|(d, _, _)| step < d).unwrap_or(true) {
                    best = Some((step, edge, tile));
                }
                break;
            }
        }
    }
    match best {
        Some((_, edge, tile)) => (Some(edge), Some(tile)),
        None => (None, None),
    }
}

fn is_network_road(chunk_map: &ChunkMap, brain: &SettlementBrain, tile: (i32, i32)) -> bool {
    brain.road_tiles.contains(&tile)
        || chunk_map.tile_kind_at(tile.0, tile.1) == Some(TileKind::Road)
}

fn distance_to_road_network(
    chunk_map: &ChunkMap,
    brain: &SettlementBrain,
    tile: (i32, i32),
    radius: i32,
) -> Option<i32> {
    let mut best: Option<i32> = None;
    for dy in -radius..=radius {
        for dx in -radius..=radius {
            let d = dx.abs().max(dy.abs());
            if best.map_or(false, |b| d >= b) {
                continue;
            }
            let probe = (tile.0 + dx, tile.1 + dy);
            if is_network_road(chunk_map, brain, probe) {
                best = Some(d);
            }
        }
    }
    best
}

fn desire_path_push(
    brain: &mut SettlementBrain,
    home: (i32, i32),
    faction_id: u32,
    tick: u64,
    chunk_map: &ChunkMap,
) -> Option<(u32, (i32, i32), (i32, i32))> {
    if tick.saturating_sub(brain.last_path_carve_tick) < DESIRE_PATH_INTERVAL {
        return None;
    }
    let Some((&tile, &_heat)) = brain
        .traffic_heat
        .iter()
        .filter(|(tile, heat)| **heat >= 80 && cheb(**tile, home) >= 6)
        .max_by_key(|(_, heat)| **heat)
    else {
        return None;
    };
    if road_near(chunk_map, tile, 3) {
        return None;
    }
    // Never run a desire path through a farm field. The carve chokepoint
    // (`road_carve_system`) would skip the tilled tiles anyway, but dropping
    // the target here avoids queueing a doomed Bresenham across the belt.
    if brain
        .parcels
        .iter()
        .filter(|p| p.district_hint == Some(DistrictKind::Agricultural))
        .any(|p| p.rect().contains(tile.0, tile.1))
    {
        return None;
    }
    brain.last_path_carve_tick = tick;
    Some((faction_id, tile, home))
}

fn road_near(chunk_map: &ChunkMap, tile: (i32, i32), radius: i32) -> bool {
    for dy in -radius..=radius {
        for dx in -radius..=radius {
            if chunk_map.tile_kind_at(tile.0 + dx, tile.1 + dy) == Some(TileKind::Road) {
                return true;
            }
        }
    }
    false
}

fn nearest_water_access(
    chunk_map: &ChunkMap,
    home: (i32, i32),
    radius: i32,
) -> Option<((i32, i32), bool)> {
    let mut best: Option<(i32, (i32, i32), bool)> = None;
    for y in home.1 - radius..=home.1 + radius {
        for x in home.0 - radius..=home.0 + radius {
            let Some(kind) = chunk_map.tile_kind_at(x, y) else {
                continue;
            };
            if !kind.is_water_like() {
                continue;
            }
            let fresh = kind.is_freshwater();
            let d = cheb((x, y), home) + if fresh { 0 } else { 12 };
            if best.map(|(bd, _, _)| d < bd).unwrap_or(true) {
                best = Some((d, (x, y), fresh));
            }
        }
    }
    best.map(|(_, tile, fresh)| (tile, fresh))
}

fn best_fertile_tile(chunk_map: &ChunkMap, home: (i32, i32), radius: i32) -> Option<(i32, i32)> {
    let mut best: Option<(u8, i32, (i32, i32))> = None;
    for y in home.1 - radius..=home.1 + radius {
        for x in home.0 - radius..=home.0 + radius {
            let fertility = chunk_map.tile_fertility_at(x, y).unwrap_or(0);
            if fertility < 130 {
                continue;
            }
            let d = cheb((x, y), home);
            if best
                .map(|(bf, bd, _)| (fertility, -d) > (bf, -bd))
                .unwrap_or(true)
            {
                best = Some((fertility, d, (x, y)));
            }
        }
    }
    best.map(|(_, _, tile)| tile)
}

fn best_high_ground(chunk_map: &ChunkMap, home: (i32, i32), radius: i32) -> Option<(i32, i32)> {
    let home_z = chunk_map.ground_z_at(home.0, home.1);
    let mut best: Option<(i32, i32, (i32, i32))> = None;
    for y in (home.1 - radius..=home.1 + radius).step_by(3) {
        for x in (home.0 - radius..=home.0 + radius).step_by(3) {
            // High-ground search ranks real terrain, not water surface.
            let z = chunk_map.ground_z_at(x, y);
            let gain = z - home_z;
            if gain <= 0 {
                continue;
            }
            let d = cheb((x, y), home);
            if best
                .map(|(bg, bd, _)| (gain, -d) > (bg, -bd))
                .unwrap_or(true)
            {
                best = Some((gain, d, (x, y)));
            }
        }
    }
    best.map(|(_, _, tile)| tile)
}

fn nearest_material_patch(
    chunk_map: &ChunkMap,
    home: (i32, i32),
    radius: i32,
) -> Option<(i32, i32)> {
    let mut best: Option<(i32, (i32, i32))> = None;
    for y in home.1 - radius..=home.1 + radius {
        for x in home.0 - radius..=home.0 + radius {
            let Some(kind) = chunk_map.tile_kind_at(x, y) else {
                continue;
            };
            if !(kind == TileKind::Forest || kind.is_stone_like()) {
                continue;
            }
            let d = cheb((x, y), home);
            if best.map(|(bd, _)| d < bd).unwrap_or(true) {
                best = Some((d, (x, y)));
            }
        }
    }
    best.map(|(_, tile)| tile)
}

fn material_anchor_bonus(brain: &SettlementBrain, tile: (i32, i32)) -> f32 {
    brain
        .anchors
        .iter()
        .filter(|a| {
            matches!(
                a.kind,
                SettlementAnchorKind::MaterialPatch | SettlementAnchorKind::Workshop
            )
        })
        .map(|a| 1.0 / (1.0 + cheb(a.tile, tile) as f32 * 0.15))
        .fold(0.0, f32::max)
}

fn local_slope(chunk_map: &ChunkMap, tile: (i32, i32)) -> i32 {
    // Terrain slope = bed-height deltas (water surface is flat and would
    // mask the real grade).
    let z = chunk_map.ground_z_at(tile.0, tile.1);
    [(1, 0), (-1, 0), (0, 1), (0, -1)]
        .iter()
        .map(|(dx, dy)| (chunk_map.ground_z_at(tile.0 + dx, tile.1 + dy) - z).abs())
        .max()
        .unwrap_or(0)
}

fn count_near(map: &AHashMap<(i32, i32), Entity>, home: (i32, i32), radius: i32) -> usize {
    map.keys()
        .filter(|&&tile| cheb(tile, home) <= radius)
        .count()
}

fn fresh_water_within(chunk_map: &ChunkMap, home: (i32, i32), radius: i32) -> bool {
    for r in 0..=radius {
        for dy in -r..=r {
            for dx in -r..=r {
                if dx.abs().max(dy.abs()) != r {
                    continue;
                }
                let tile = (home.0 + dx, home.1 + dy);
                if let Some(kind) = chunk_map.tile_kind_at(tile.0, tile.1) {
                    if matches!(kind, TileKind::River | TileKind::Bridge) {
                        return true;
                    }
                }
            }
        }
    }
    false
}

fn nearest_fresh_or_well_distance(
    chunk_map: &ChunkMap,
    well_map: &AHashMap<(i32, i32), Entity>,
    home: (i32, i32),
    radius: i32,
) -> i32 {
    for r in 0..=radius {
        for dy in -r..=r {
            for dx in -r..=r {
                if dx.abs().max(dy.abs()) != r {
                    continue;
                }
                let tile = (home.0 + dx, home.1 + dy);
                if well_map.contains_key(&tile) {
                    return r;
                }
                if let Some(kind) = chunk_map.tile_kind_at(tile.0, tile.1) {
                    if matches!(kind, TileKind::River | TileKind::Bridge) {
                        return r;
                    }
                }
            }
        }
    }
    radius
}

fn union_rect(a: TileRect, b: TileRect) -> TileRect {
    let x0 = a.x0.min(b.x0);
    let y0 = a.y0.min(b.y0);
    let x1 = (a.x0 + a.w as i32).max(b.x0 + b.w as i32);
    let y1 = (a.y0 + a.h as i32).max(b.y0 + b.h as i32);
    TileRect::new(x0, y0, (x1 - x0).max(1) as u16, (y1 - y0).max(1) as u16)
}

fn rects_overlap(a: TileRect, b: TileRect) -> bool {
    a.x0 < b.x0 + b.w as i32
        && a.x0 + a.w as i32 > b.x0
        && a.y0 < b.y0 + b.h as i32
        && a.y0 + a.h as i32 > b.y0
}

fn zone_priority(kind: ZoneKind, faction: &FactionData) -> u8 {
    match kind {
        ZoneKind::Residential => 200,
        ZoneKind::Agricultural => 110,
        ZoneKind::Crafting => 130,
        ZoneKind::Civic => 180,
        ZoneKind::Defense => (120 + faction.culture.defensive / 3).min(240),
        ZoneKind::Storage => 150,
        ZoneKind::Sacred => (110 + faction.culture.ceremonial / 4).min(220),
        ZoneKind::Market => (100 + faction.culture.mercantile / 4).min(220),
    }
}

fn zone_capacity(kind: ZoneKind, members: u32) -> u8 {
    match kind {
        ZoneKind::Residential => members.clamp(2, 32) as u8,
        ZoneKind::Agricultural => (members / 2).clamp(2, 24) as u8,
        ZoneKind::Defense => 16,
        _ => 4,
    }
}

fn organic_street_spine(faction: &FactionData, brain: &SettlementBrain) -> StreetSpine {
    if !faction.community_has(PERM_SETTLEMENT) || brain.road_segments.is_empty() {
        return StreetSpine::None;
    }
    StreetSpine::Spokes {
        plaza: faction.home_tile,
        segments: brain.road_segments.clone(),
    }
}

fn layout_hash<F: SurveyFactionView>(faction: &F, brain: &SettlementBrain) -> u64 {
    let phase = brain.phase as u64;
    let pop_bucket = (faction.member_count() / 5) as u64;
    let road_hash = brain
        .road_segments
        .iter()
        .fold(0u64, |acc, seg| acc ^ segment_hash(*seg));
    brain.seed
        ^ (phase << 56)
        ^ (pop_bucket << 48)
        ^ ((brain.parcels.len() as u64).min(255) << 40)
        ^ road_hash.rotate_left(7)
        ^ ((faction.culture().density as u64) << 24)
        ^ ((faction.culture().defensive as u64) << 16)
}

fn segment_hash(seg: StreetSegment) -> u64 {
    let tier = match seg.tier {
        StreetTier::Primary => 1u64,
        StreetTier::Secondary => 2u64,
        StreetTier::Alley => 3u64,
    };
    ((seg.start.0 as u32 as u64) << 32)
        ^ (seg.start.1 as u32 as u64)
        ^ ((seg.end.0 as u32 as u64).rotate_left(13))
        ^ ((seg.end.1 as u32 as u64).rotate_left(29))
        ^ (tier << 60)
}

#[inline]
fn cheb(a: (i32, i32), b: (i32, i32)) -> i32 {
    (a.0 - b.0).abs().max((a.1 - b.1).abs())
}

#[inline]
fn midpoint(a: (i32, i32), b: (i32, i32)) -> (i32, i32) {
    ((a.0 + b.0) / 2, (a.1 + b.1) / 2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::simulation::construction::{
        CampfireEntry, CampfireMap, HearthRole,
    };

    // ── Hearth pressure formulas (Neolithic+ fix) ────────────────────────

    #[test]
    fn pressure_target_paleo_20_pop_is_4() {
        assert_eq!(desired_public_hearths(Era::Paleolithic, 20), 4);
    }

    #[test]
    fn pressure_target_meso_20_pop_is_4() {
        assert_eq!(desired_public_hearths(Era::Mesolithic, 20), 4);
    }

    #[test]
    fn pressure_target_neolithic_20_pop_is_1() {
        assert_eq!(desired_public_hearths(Era::Neolithic, 20), 1);
    }

    #[test]
    fn pressure_target_chalcolithic_20_pop_is_1() {
        assert_eq!(desired_public_hearths(Era::Chalcolithic, 20), 1);
    }

    #[test]
    fn pressure_target_bronze_20_pop_is_1() {
        assert_eq!(desired_public_hearths(Era::BronzeAge, 20), 1);
    }

    #[test]
    fn public_hearth_role_for_era_split() {
        assert_eq!(public_hearth_role_for_era(Era::Paleolithic), HearthRole::Camp);
        assert_eq!(public_hearth_role_for_era(Era::Mesolithic), HearthRole::Camp);
        assert_eq!(public_hearth_role_for_era(Era::Neolithic), HearthRole::Civic);
        assert_eq!(public_hearth_role_for_era(Era::Chalcolithic), HearthRole::Civic);
        assert_eq!(public_hearth_role_for_era(Era::BronzeAge), HearthRole::Civic);
    }

    #[test]
    fn domestic_hearths_do_not_count_toward_civic_pressure() {
        // Stage three hearths at the home tile: two Domestic (Longhouse
        // interiors) and zero Civic. Counting under the Neolithic+ rule
        // must return 0, so pressure for one Civic hearth still fires.
        let home = (0, 0);
        let mut map = CampfireMap::default();
        let dummy_entity = bevy::ecs::entity::Entity::from_raw(1);
        map.0.insert(
            home,
            CampfireEntry {
                entity: dummy_entity,
                role: HearthRole::Domestic,
            },
        );
        map.0.insert(
            (1, 0),
            CampfireEntry {
                entity: bevy::ecs::entity::Entity::from_raw(2),
                role: HearthRole::Domestic,
            },
        );
        assert_eq!(
            map.count_role_near(home, 32, HearthRole::Civic),
            0,
            "Domestic interior hearths must not satisfy Civic pressure",
        );
        assert_eq!(map.count_role_near(home, 32, HearthRole::Domestic), 2);
        assert_eq!(map.count_any_near(home, 32), 2);
    }

    fn dummy_faction(home: (i32, i32), members: u32) -> FactionData {
        let mut registry = FactionRegistry::default();
        let id = registry.create_faction(home);
        let mut faction = registry.factions.remove(&id).unwrap();
        faction.member_count = members;
        faction
    }

    /// Force-adopt a tech on a test faction: flips chief-Aware *and*
    /// community-Adopted so civic gates that now read the adoption layer
    /// (not chief-Aware) fire as the test expects.
    fn force_adopt(faction: &mut FactionData, tech: crate::simulation::technology::TechId) {
        // `community_has` now reads `buildable_techs` (the poster-pool
        // surface), not the legacy adoption layer. Set both so tests that
        // inspect either stay correct.
        faction.techs.unlock(tech);
        faction.buildable_techs.unlock(tech);
        faction.tech_adoption[tech as usize] =
            crate::simulation::technology_adoption::AdoptionStage::Adopted;
    }

    #[test]
    fn phase_tracks_population_and_permanent_settlement() {
        let mut faction = dummy_faction((0, 0), 20);
        assert_eq!(phase_for(&faction, 20), SettlementPhase::Camp);
        force_adopt(&mut faction, PERM_SETTLEMENT);
        assert_eq!(phase_for(&faction, 8), SettlementPhase::Hamlet);
        assert_eq!(phase_for(&faction, 20), SettlementPhase::Village);
    }

    #[test]
    fn parcel_suitability_prefers_fertile_fields() {
        let suitability = ParcelSuitability {
            residential: 0.3,
            agricultural: 1.2,
            crafting: 0.1,
            civic: 0.2,
            defense: 0.0,
            storage: 0.2,
            sacred: 0.1,
            market: 0.1,
        };
        assert_eq!(
            best_district_for_parcel(&suitability),
            Some(DistrictKind::Agricultural)
        );
    }

    #[test]
    fn material_penalty_reads_required_goods() {
        let goods = required_goods(OrganicBuildKind::Hut(WallMaterial::WattleDaub));
        assert!(!goods.is_empty());
    }

    #[test]
    fn planned_road_tiles_protect_spine_interior() {
        let segment = StreetSegment {
            start: (-3, 0),
            end: (3, 0),
            tier: StreetTier::Primary,
        };
        let tiles = road_tiles_for_segments(&[segment]);
        assert!(!tiles.contains(&(-3, 0)));
        assert!(!tiles.contains(&(3, 0)));
        assert!(tiles.contains(&(0, 0)));
        assert!(tiles.contains(&(2, 0)));
    }

    #[test]
    fn permanent_hamlet_gets_road_skeleton_before_lots() {
        let mut faction = dummy_faction((0, 0), 8);
        force_adopt(&mut faction, PERM_SETTLEMENT);
        let mut brain = SettlementBrain::new(SettlementId(1), 1, 42);
        brain.phase = SettlementPhase::Hamlet;

        let map = ChunkMap::default();
        let segments = build_road_network(&faction, &brain, &map, &[]);
        let tiles = road_tiles_for_segments(&segments);

        assert!(!segments.is_empty());
        assert!(tiles.contains(&(1, 0)));
        assert!(tiles.contains(&(-1, 0)));
    }

    fn flat_chunk(kind: crate::world::tile::TileKind) -> crate::world::chunk::Chunk {
        let surface_z =
            Box::new([[0i8; crate::world::chunk::CHUNK_SIZE]; crate::world::chunk::CHUNK_SIZE]);
        let surface_kind =
            Box::new([[kind; crate::world::chunk::CHUNK_SIZE]; crate::world::chunk::CHUNK_SIZE]);
        let surface_fertility =
            Box::new([[8u8; crate::world::chunk::CHUNK_SIZE]; crate::world::chunk::CHUNK_SIZE]);
        crate::world::chunk::Chunk::new(surface_z, surface_kind, surface_fertility)
    }

    fn grass_map() -> ChunkMap {
        let mut m = ChunkMap::default();
        for cy in -1..=1 {
            for cx in -1..=1 {
                m.0.insert(
                    crate::world::chunk::ChunkCoord(cx, cy),
                    flat_chunk(crate::world::tile::TileKind::Grass),
                );
            }
        }
        m
    }

    fn write_river_at(m: &mut ChunkMap, tiles: &[(i32, i32)]) {
        for &(x, y) in tiles {
            m.set_tile(
                x,
                y,
                0,
                crate::world::tile::TileData {
                    kind: crate::world::tile::TileKind::River,
                    ..Default::default()
                },
            );
        }
    }

    #[test]
    fn detect_runs_finds_two_tile_crossing() {
        let mut m = grass_map();
        // 2-tile river at x=0 and x=1, rest grass.
        write_river_at(&mut m, &[(0, 0), (1, 0)]);
        let trace: Vec<(i32, i32)> = (-3..=4).map(|x| (x, 0)).collect();
        let runs = detect_bridge_runs_in_trace(&m, &trace);
        assert_eq!(runs.len(), 1, "should detect one crossing");
        let (s, e) = runs[0];
        assert_eq!((trace[s], trace[e]), ((0, 0), (1, 0)));
    }

    #[test]
    fn detect_runs_rejects_overlong_crossing() {
        let mut m = grass_map();
        // 5-tile river — exceeds MAX_BRIDGE_SPAN = 4.
        let r: Vec<(i32, i32)> = (0..5).map(|x| (x, 0)).collect();
        write_river_at(&mut m, &r);
        let trace: Vec<(i32, i32)> = (-3..=8).map(|x| (x, 0)).collect();
        let runs = detect_bridge_runs_in_trace(&m, &trace);
        assert!(runs.is_empty(), "5-tile river should not be bridged");
    }

    #[test]
    fn pre_bridge_road_network_skips_river_crossing_spokes() {
        let mut faction = dummy_faction((0, 0), 8);
        force_adopt(&mut faction, PERM_SETTLEMENT);
        let mut brain = SettlementBrain::new(SettlementId(1), 1, 42);
        brain.phase = SettlementPhase::Hamlet;

        // NS river right through home — the default E-W primary spoke
        // would cross it; pre-tech, it must be dropped.
        let mut map = grass_map();
        let r: Vec<(i32, i32)> = (-30..=30).map(|y| (0, y)).collect();
        write_river_at(&mut map, &r);

        let segments = build_road_network(&faction, &brain, &map, &[]);
        for seg in &segments {
            assert!(
                !trace_crosses_river(&map, *seg),
                "pre-bridge planner produced a river-crossing segment: {:?}",
                seg
            );
        }
    }

    #[test]
    fn post_bridge_road_network_keeps_river_crossings() {
        let mut faction = dummy_faction((0, 0), 8);
        force_adopt(&mut faction, PERM_SETTLEMENT);
        force_adopt(&mut faction, BRIDGE_BUILDING);
        let mut brain = SettlementBrain::new(SettlementId(1), 1, 42);
        brain.phase = SettlementPhase::Hamlet;

        let mut map = grass_map();
        let r: Vec<(i32, i32)> = (-30..=30).map(|y| (0, y)).collect();
        write_river_at(&mut map, &r);

        let segments = build_road_network(&faction, &brain, &map, &[]);
        let any_crossing = segments.iter().any(|seg| trace_crosses_river(&map, *seg));
        assert!(
            any_crossing,
            "post-bridge planner should retain crossings so the emitter can bridge them"
        );
    }

    #[test]
    fn parallel_spine_promoted_along_ns_river() {
        let mut faction = dummy_faction((0, 0), 8);
        force_adopt(&mut faction, PERM_SETTLEMENT);
        let mut brain = SettlementBrain::new(SettlementId(1), 1, 42);
        brain.phase = SettlementPhase::Hamlet;
        // Hamlet phase only stamps the FIRST primary spoke (the second one
        // is gated on `member_count >= 10`). With an NS river nearby, that
        // first spoke must run N-S (parallel to water), not E-W.

        let mut map = grass_map();
        let r: Vec<(i32, i32)> = (-30..=30).map(|y| (5, y)).collect();
        write_river_at(&mut map, &r);

        let segments = build_road_network(&faction, &brain, &map, &[]);
        // Exactly one Primary spoke survives at hamlet phase; assert it's NS.
        let primaries: Vec<&StreetSegment> = segments
            .iter()
            .filter(|s| s.tier == StreetTier::Primary)
            .collect();
        assert!(!primaries.is_empty(), "expected a primary spine");
        let ns_aligned = primaries
            .iter()
            .any(|s| s.start.0 == s.end.0 && s.start.1 != s.end.1);
        assert!(
            ns_aligned,
            "primary spine should be N-S parallel to the river; got {:?}",
            primaries
        );
    }

    #[test]
    fn detect_runs_skips_run_without_two_banks() {
        let mut m = grass_map();
        write_river_at(&mut m, &[(0, 0)]);
        // Trace starts inside the river — no preceding bank tile.
        let trace: Vec<(i32, i32)> = (0..=4).map(|x| (x, 0)).collect();
        let runs = detect_bridge_runs_in_trace(&m, &trace);
        assert!(runs.is_empty());
    }

    /// Settlement realism: the perpendicular cross fires only when
    /// (a) member_count ≥ 16 (dense), OR
    /// (b) a WaterAccess/Field/Market anchor projects > 6 tiles off
    ///     the spine axis.
    /// pop=12 with no off-axis anchors stays single-spine — the cross is
    /// no longer a population-12 freebie.
    #[test]
    fn village_phase_emits_cross_when_dense_or_offaxis_anchor() {
        let mut faction = dummy_faction((0, 0), 16);
        force_adopt(&mut faction, PERM_SETTLEMENT);
        let mut brain = SettlementBrain::new(SettlementId(1), 1, 42);
        brain.phase = SettlementPhase::Village;
        let map = grass_map();
        let offsets: Vec<(i32, i32)> = (0..16).map(|i| (i - 8, 0)).collect();

        // Dense village (pop=16+, no anchors) — cross fires from density.
        let segs = build_road_network(&faction, &brain, &map, &offsets);
        assert!(
            segs.len() >= 2,
            "dense village @ pop=16 expected ≥2 segments (spine + cross), got {}",
            segs.len()
        );

        // Pop=12, no off-axis anchor — single spine, no cross.
        faction.member_count = 12;
        let segs_small = build_road_network(&faction, &brain, &map, &offsets[..12]);
        assert_eq!(
            segs_small.len(),
            1,
            "village @ pop=12 with no off-axis anchor should be spine-only"
        );

        // Pop=12 + a Field anchor 10 tiles off the EW spine (north of home)
        // — cross fires from the off-axis demand.
        faction.member_count = 12;
        let mut brain2 = brain.clone();
        brain2.anchors.push(SettlementAnchor {
            kind: SettlementAnchorKind::Field,
            tile: (0, 10),
            weight: 1.0,
        });
        let segs_anchor = build_road_network(&faction, &brain2, &map, &offsets[..12]);
        assert!(
            segs_anchor.len() >= 2,
            "pop=12 + off-axis Field anchor should emit a perpendicular spur"
        );
    }

    /// Urban-phase parallel streets must respect the 12-tile minimum block
    /// spacing (≈18 m at 1.5 m/tile — the floor for fitting one row of
    /// houses with yards between two parallel roads). The shipped Urban
    /// schedule uses ±12 / ±24 offsets along the primary axis; check that
    /// every adjacent pair along the perpendicular axis is ≥12 apart.
    #[test]
    fn urban_block_spacing_meets_realistic_floor() {
        let mut faction = dummy_faction((0, 0), 100);
        force_adopt(&mut faction, PERM_SETTLEMENT);
        let mut brain = SettlementBrain::new(SettlementId(1), 1, 42);
        brain.phase = SettlementPhase::Urban;
        let map = grass_map();
        let offsets: Vec<(i32, i32)> = (0..30).map(|i| (i - 15, 0)).collect();

        let segs = build_road_network(&faction, &brain, &map, &offsets);
        // Urban → primary spine + 4 parallel offsets (±12, ±24) along axis.
        assert!(
            segs.len() >= 5,
            "urban expected ≥5 segments, got {}",
            segs.len()
        );

        // Collect Y-coordinate of horizontal (EW-axis) primary-axis spines:
        // segments whose start.y == end.y. Sort, then check spacing.
        let mut ys: Vec<i32> = segs
            .iter()
            .filter(|s| s.start.1 == s.end.1)
            .map(|s| s.start.1)
            .collect();
        ys.sort();
        ys.dedup();
        if ys.len() >= 2 {
            for w in ys.windows(2) {
                let gap = (w[1] - w[0]).abs();
                assert!(
                    gap >= 12,
                    "urban parallel street spacing {} violates 12-tile floor (ys={:?})",
                    gap,
                    ys
                );
            }
        }
    }

    /// Every Residential parcel emitted by `build_parcels_road_driven`
    /// must have `frontage_edge.is_some()` and `access_tile.is_some()` —
    /// the road-tile-driven sweep derives candidate rects from cardinal
    /// neighbours of road tiles, so frontage is inherent. This is the
    /// invariant the seed pipeline relies on for door placement.
    #[test]
    fn permanent_residential_parcels_have_frontage_invariant() {
        use crate::economy::market::SettlementMarket;
        let mut faction = dummy_faction((0, 0), 30);
        force_adopt(&mut faction, PERM_SETTLEMENT);
        let mut brain = SettlementBrain::new(SettlementId(1), 1, 42);
        brain.phase = SettlementPhase::Chiefdom;
        let map = grass_map();
        let offsets: Vec<(i32, i32)> = (0..30).map(|i| (i - 15, 0)).collect();

        // Populate the road skeleton so build_parcels takes the
        // road-driven branch.
        brain.road_segments = build_road_network(&faction, &brain, &map, &offsets);
        brain.road_tiles = road_tiles_for_segments(&brain.road_segments);
        assert!(
            !brain.road_tiles.is_empty(),
            "chiefdom should have a road skeleton"
        );

        let settlement = Settlement {
            id: SettlementId(1),
            owner_faction: 1,
            market_tile: (0, 0),
            founding_tick: 0,
            name: "Test".into(),
            treasury: 0.0,
            market: SettlementMarket::default(),
            peak_population: 30,
            locality: None,
        };

        let parcels = build_parcels(
            &faction,
            &settlement,
            &brain,
            &map,
            &SurveyStructureSnapshot::default(),
            &[],
        );
        let residential: Vec<_> = parcels
            .iter()
            .filter(|p| p.district_hint == Some(DistrictKind::Residential))
            .collect();
        assert!(
            !residential.is_empty(),
            "chiefdom with 30 members + road network should produce ≥1 Residential parcel"
        );
        for p in &residential {
            assert!(
                p.frontage_edge.is_some(),
                "Residential parcel #{} lacks frontage_edge (rect={:?})",
                p.id,
                p.rect()
            );
            assert!(
                p.access_tile.is_some(),
                "Residential parcel #{} lacks access_tile",
                p.id
            );
        }
    }

    fn fertile_chunk(kind: crate::world::tile::TileKind) -> crate::world::chunk::Chunk {
        let surface_z =
            Box::new([[0i8; crate::world::chunk::CHUNK_SIZE]; crate::world::chunk::CHUNK_SIZE]);
        let surface_kind =
            Box::new([[kind; crate::world::chunk::CHUNK_SIZE]; crate::world::chunk::CHUNK_SIZE]);
        // Fertility 200/255 ≈ 0.78 — comfortably above AG_BELT_MIN_FERT.
        let surface_fertility =
            Box::new([[200u8; crate::world::chunk::CHUNK_SIZE]; crate::world::chunk::CHUNK_SIZE]);
        crate::world::chunk::Chunk::new(surface_z, surface_kind, surface_fertility)
    }

    fn fertile_grass_map() -> ChunkMap {
        let mut m = ChunkMap::default();
        for cy in -2..=3 {
            for cx in -2..=3 {
                m.0.insert(
                    crate::world::chunk::ChunkCoord(cx, cy),
                    fertile_chunk(crate::world::tile::TileKind::Grass),
                );
            }
        }
        m
    }

    fn ag_test_settlement() -> Settlement {
        use crate::economy::market::SettlementMarket;
        Settlement {
            id: SettlementId(1),
            owner_faction: 1,
            market_tile: (0, 0),
            founding_tick: 0,
            name: "AgTest".into(),
            treasury: 0.0,
            market: SettlementMarket::default(),
            peak_population: 30,
            locality: None,
        }
    }

    /// The ag belt is allocated as 16×16 blocks OUTSIDE the built-up
    /// footprint, self-anchored on home (NOT the old home-biased Field
    /// anchor), with no near-home fallback. Road access is now soft, so the
    /// belt fires even without a nearby road.
    #[test]
    fn ag_belt_outside_footprint() {
        let mut faction = dummy_faction((0, 0), 30);
        force_adopt(&mut faction, PERM_SETTLEMENT);
        force_adopt(&mut faction, CROP_CULTIVATION);
        let mut brain = SettlementBrain::new(SettlementId(1), 1, 42);
        brain.phase = SettlementPhase::Chiefdom;
        // Only a Civic disc — NO Agricultural district. The belt must still
        // fire (it no longer depends on a Field anchor) and stay clear of
        // the built-up footprint. No road tiles either (access is soft).
        brain.districts = vec![DistrictInfluence {
            kind: DistrictKind::Civic,
            centre: (0, 0),
            radius: 5,
            weight: 1.0,
        }];
        let map = fertile_grass_map();
        let settlement = ag_test_settlement();

        let parcels = build_ag_belt(&faction, &settlement, &brain, &map, &[], 100, MAX_PARCELS, &[]);
        assert!(
            !parcels.is_empty(),
            "robust belt must yield ≥1 ag block on fertile land even with no \
             Field anchor and no road"
        );

        // Civic disc (home, r=5) inflated by BELT_CLEARANCE=3 → keep-out box.
        let civic_keepout = TileRect::new(-8, -8, 17, 17);
        for (i, p) in parcels.iter().enumerate() {
            assert_eq!(p.district_hint, Some(DistrictKind::Agricultural));
            assert!(p.id >= 100, "belt ids continue the parcel sequence");
            assert!(
                p.frontage_edge.is_none(),
                "ag fields are not road-fronted (parcel {})",
                p.id
            );
            let r = p.rect();
            assert_eq!((r.w, r.h), (16, 16), "ag parcels are 16×16");
            assert!(
                !rects_overlap(r, civic_keepout),
                "ag parcel {} overlaps the built-up footprint {:?}",
                p.id,
                r
            );
            // Strictly outside the civic radius (not hugging the base).
            let cx = r.x0 + r.w as i32 / 2;
            let cy = r.y0 + r.h as i32 / 2;
            assert!(
                (cx).abs().max(cy.abs()) > 5,
                "ag parcel {} centre {:?} is inside the civic core",
                p.id,
                (cx, cy)
            );
            for (j, q) in parcels.iter().enumerate() {
                if i != j {
                    assert!(
                        !rects_overlap(r, q.rect()),
                        "ag parcels {} and {} overlap",
                        p.id,
                        q.id
                    );
                }
            }
        }
    }

    /// Belt allocation is deterministic: same brain/map → identical rects.
    #[test]
    fn ag_belt_deterministic() {
        let mut faction = dummy_faction((0, 0), 30);
        force_adopt(&mut faction, PERM_SETTLEMENT);
        force_adopt(&mut faction, CROP_CULTIVATION);
        let mut brain = SettlementBrain::new(SettlementId(1), 1, 42);
        brain.phase = SettlementPhase::Chiefdom;
        brain.districts = vec![DistrictInfluence {
            kind: DistrictKind::Civic,
            centre: (0, 0),
            radius: 5,
            weight: 1.0,
        }];
        let map = fertile_grass_map();
        let settlement = ag_test_settlement();

        let a = build_ag_belt(&faction, &settlement, &brain, &map, &[], 0, MAX_PARCELS, &[]);
        let b = build_ag_belt(&faction, &settlement, &brain, &map, &[], 0, MAX_PARCELS, &[]);
        assert!(!a.is_empty());
        let rects_a: Vec<TileRect> = a.iter().map(|p| p.rect()).collect();
        let rects_b: Vec<TileRect> = b.iter().map(|p| p.rect()).collect();
        assert_eq!(
            rects_a, rects_b,
            "build_ag_belt must be deterministic across calls"
        );
    }

    /// Sticky-belt regression: when `build_ag_belt` is given a committed
    /// Ag plot rect that sits at an *un-fertile-favoured* location, that
    /// committed rect must still appear in the output (pre-accepted),
    /// even if the scoring system would otherwise prefer different
    /// blocks. The fertility-driven scan is only allowed to fill any
    /// remaining demand, never to relocate committed plots.
    #[test]
    fn build_ag_belt_preserves_committed_rects() {
        let mut faction = dummy_faction((0, 0), 30);
        force_adopt(&mut faction, PERM_SETTLEMENT);
        force_adopt(&mut faction, CROP_CULTIVATION);
        let mut brain = SettlementBrain::new(SettlementId(1), 1, 42);
        brain.phase = SettlementPhase::Chiefdom;
        brain.anchors = vec![SettlementAnchor {
            kind: SettlementAnchorKind::Field,
            tile: (10, 10),
            weight: 1.0,
        }];
        let map = fertile_grass_map();
        let settlement = ag_test_settlement();

        // An off-axis committed rect that the home-anchored lattice would
        // *not* pick first. Its placement must survive the call.
        let committed = TileRect::new(40, 40, 16, 16);
        let belt = build_ag_belt(
            &faction,
            &settlement,
            &brain,
            &map,
            &[],
            500,
            MAX_PARCELS,
            &[committed],
        );

        assert!(
            belt.iter().any(|p| p.rect() == committed),
            "committed Ag rect must be pre-accepted into the belt regardless of scoring; \
             got {:?}",
            belt.iter().map(|p| p.rect()).collect::<Vec<_>>()
        );
    }

    /// Regression: with no belt/frontier Agricultural parcel, the compat
    /// plan must NOT synthesize a home-centred Agricultural zone from the
    /// district-broad or legacy `build_settlement_plan` fallback (that carved
    /// farms all over the base). Belt parcels DO flow through verbatim.
    #[test]
    fn compat_plan_no_near_home_ag_fallback() {
        let mut faction = dummy_faction((0, 0), 30);
        force_adopt(&mut faction, PERM_SETTLEMENT);
        force_adopt(&mut faction, CROP_CULTIVATION);

        // No parcels, no Agricultural district → no Agricultural zone.
        let mut brain = SettlementBrain::new(SettlementId(1), 1, 42);
        brain.phase = SettlementPhase::Chiefdom;
        let plan = compat_plan_from_brain(1, &faction, 0, &brain);
        assert!(
            !plan.zones.iter().any(|z| z.kind == ZoneKind::Agricultural),
            "no belt parcel ⇒ NO Agricultural zone (no near-home fallback)"
        );

        // A belt parcel far from home flows through unchanged.
        let belt = TileRect::new(80, 80, 16, 16);
        brain.parcels.push(Parcel {
            id: 0,
            shape: ParcelShape::Rect(belt),
            frontage_edge: None,
            access_tile: None,
            holder: TenureHolder::State { faction_id: 1 },
            district_hint: Some(DistrictKind::Agricultural),
            suitability: ParcelSuitability::default(),
        });
        let plan2 = compat_plan_from_brain(1, &faction, 0, &brain);
        let ag: Vec<_> = plan2
            .zones
            .iter()
            .filter(|z| z.kind == ZoneKind::Agricultural)
            .collect();
        assert_eq!(
            ag.len(),
            1,
            "the single belt parcel → one Agricultural zone"
        );
        assert_eq!(ag[0].rect, belt, "ag zone uses the belt rect, not home");
    }

    /// `survey_one_settlement`'s expensive sub-functions
    /// (`build_road_network` + `build_parcels` + `road_tiles_for_segments`)
    /// are pure on `(faction, brain, chunk_map, member_offsets)`. Calling
    /// them twice with the same inputs must produce identical outputs —
    /// this is the determinism guarantee that lets the OnEnter kickoff
    /// survey and the FixedUpdate async survey compute converge on the
    /// same brain.
    #[test]
    fn survey_subfunctions_are_deterministic() {
        let mut faction = dummy_faction((0, 0), 16);
        force_adopt(&mut faction, PERM_SETTLEMENT);
        let mut brain = SettlementBrain::new(SettlementId(1), 1, 42);
        brain.phase = SettlementPhase::Village;
        let map = grass_map();
        let member_offsets: Vec<(i32, i32)> = (0..16).map(|i| (i - 8, 0)).collect();

        let segs_a = build_road_network(&faction, &brain, &map, &member_offsets);
        let segs_b = build_road_network(&faction, &brain, &map, &member_offsets);
        assert_eq!(
            segs_a.len(),
            segs_b.len(),
            "road network must be deterministic across re-runs"
        );
        for (a, b) in segs_a.iter().zip(segs_b.iter()) {
            assert_eq!(a.start, b.start);
            assert_eq!(a.end, b.end);
        }

        let tiles_a = road_tiles_for_segments(&segs_a);
        let tiles_b = road_tiles_for_segments(&segs_b);
        assert_eq!(tiles_a, tiles_b);
    }

    /// Settlement realism: Chiefdom drops the symmetric ±18 parallels and
    /// instead emits secondary spurs toward the top-3 unmet anchors,
    /// weight-sorted, capped at 3. Anchors already covered by the spine
    /// (perp projection ≤ 2) are skipped.
    #[test]
    fn chiefdom_secondaries_target_anchors_not_symmetric_parallels() {
        let mut faction = dummy_faction((0, 0), 40);
        force_adopt(&mut faction, PERM_SETTLEMENT);
        let mut brain = SettlementBrain::new(SettlementId(1), 1, 42);
        brain.phase = SettlementPhase::Chiefdom;
        // Three off-spine anchors at varying weights.
        brain.anchors = vec![
            SettlementAnchor {
                kind: SettlementAnchorKind::Market,
                tile: (8, 12),
                weight: 3.0,
            },
            SettlementAnchor {
                kind: SettlementAnchorKind::Field,
                tile: (-10, 10),
                weight: 2.5,
            },
            SettlementAnchor {
                kind: SettlementAnchorKind::WaterAccess,
                tile: (5, -14),
                weight: 2.0,
            },
        ];
        let map = grass_map();
        // Horizontal member cluster → EW primary axis.
        let offsets: Vec<(i32, i32)> = (0..40).map(|i| (i - 20, 0)).collect();

        let segs = build_road_network(&faction, &brain, &map, &offsets);
        let secondaries: Vec<&StreetSegment> = segs
            .iter()
            .filter(|s| s.tier == StreetTier::Secondary)
            .collect();
        // Anchor-driven: cap is 3 secondaries from home.
        assert!(
            secondaries.len() <= 3,
            "chiefdom secondaries capped at 3, got {}",
            secondaries.len()
        );
        // Each secondary starts at home (or jittered home? home preserved).
        for s in &secondaries {
            assert_eq!(s.start, faction.home_tile);
        }
        // The strongest anchor (Market at weight 3.0) must show up as one
        // endpoint (modulo ±1 jitter).
        let target = (8, 12);
        let hit = secondaries
            .iter()
            .any(|s| (s.end.0 - target.0).abs() <= 1 && (s.end.1 - target.1).abs() <= 1);
        assert!(
            hit,
            "expected strongest anchor (Market @ {:?}) among secondary endpoints; got {:?}",
            target,
            secondaries.iter().map(|s| s.end).collect::<Vec<_>>()
        );
    }

    // ── Kitchen gardens: dwelling detection + garden geometry ─────────

    #[test]
    fn detect_dwellings_classifies_hut_and_longhouse() {
        let mut walls: AHashSet<(i32, i32)> = AHashSet::default();
        // 3×3 hut wall ring at (0,0) with a single door gap.
        for t in rect_perimeter_tiles(TileRect::new(0, 0, 3, 3)) {
            if t != (1, 0) {
                walls.insert(t);
            }
        }
        // 5×3 longhouse wall ring at (20,0) with a single door gap.
        for t in rect_perimeter_tiles(TileRect::new(20, 0, 5, 3)) {
            if t != (22, 0) {
                walls.insert(t);
            }
        }
        let mut found = detect_dwellings(&walls);
        found.sort_by_key(|d| d.rect.x0);
        assert_eq!(found.len(), 2, "expected one hut + one longhouse");
        assert_eq!((found[0].rect.x0, found[0].rect.y0, found[0].rect.w, found[0].rect.h), (0, 0, 3, 3));
        assert!(!found[0].is_longhouse, "3×3 ring is a hut");
        assert_eq!((found[1].rect.x0, found[1].rect.y0, found[1].rect.w, found[1].rect.h), (20, 0, 5, 3));
        assert!(found[1].is_longhouse, "5×3 ring is a longhouse");
    }

    #[test]
    fn kitchen_rect_for_edge_is_flush_to_wall() {
        let dims = |r: TileRect| (r.x0, r.y0, r.w, r.h);
        // Hut 3×3 → garden flush against the wall, 3 wide on every edge.
        let hut = TileRect::new(0, 0, 3, 3);
        assert_eq!(
            dims(kitchen_rect_for_edge(hut, TileEdge::North, 3)),
            (0, 3, 3, 3),
            "hut north garden flush at y0+h"
        );
        assert_eq!(
            dims(kitchen_rect_for_edge(hut, TileEdge::East, 3)),
            (3, 0, 3, 3),
            "hut east garden flush at x0+w"
        );
        // Longhouse 5×3 → a short-wall (E/W) garden runs 3 tiles parallel to
        // the wall (= short-wall length) and `depth` deep; never the 5-wide
        // long wall.
        let longhouse = TileRect::new(0, 0, 5, 3);
        let east = kitchen_rect_for_edge(longhouse, TileEdge::East, 4);
        assert_eq!(dims(east), (5, 0, 4, 3));
        assert_eq!(east.h, 3, "longhouse short-wall garden runs 1:1 with the 3-tile wall");
        let west = kitchen_rect_for_edge(longhouse, TileEdge::West, 4);
        assert_eq!(dims(west), (-4, 0, 4, 3));
        assert_eq!(west.h, 3);
    }

    // ─── Decrowding plan: L1/L2/L4/L5/L6 tests ────────────────────────────

    fn dense_offsets(n: usize) -> Vec<(i32, i32)> {
        (0..n as i32).map(|i| (i - (n as i32) / 2, 0)).collect()
    }

    fn build_village_brain(home: (i32, i32), members: u32) -> (FactionData, SettlementBrain, Settlement, ChunkMap, Vec<(i32, i32)>) {
        use crate::economy::market::SettlementMarket;
        let mut faction = dummy_faction(home, members);
        force_adopt(&mut faction, PERM_SETTLEMENT);
        let mut brain = SettlementBrain::new(SettlementId(1), 1, 42);
        brain.home_tile = home;
        brain.phase = SettlementPhase::Village;
        brain.commons_rect = commons_rect_for(home, brain.phase);
        let map = grass_map();
        let offsets = dense_offsets(members as usize);
        brain.road_segments = build_road_network(&faction, &brain, &map, &offsets);
        brain.road_tiles = road_tiles_for_segments(&brain.road_segments);
        brain.road_corridor_tiles = road_corridor_tiles_for_segments(&brain.road_segments);
        let settlement = Settlement {
            id: SettlementId(1),
            owner_faction: 1,
            market_tile: home,
            founding_tick: 0,
            name: "DecrowdTest".into(),
            treasury: 0.0,
            market: SettlementMarket::default(),
            peak_population: members,
            locality: None,
        };
        (faction, brain, settlement, map, offsets)
    }

    #[test]
    fn commons_keepout_blocks_residential_in_hamlet() {
        let mut faction = dummy_faction((0, 0), 12);
        force_adopt(&mut faction, PERM_SETTLEMENT);
        let mut brain = SettlementBrain::new(SettlementId(1), 1, 42);
        brain.home_tile = (0, 0);
        brain.phase = SettlementPhase::Hamlet;
        brain.commons_rect = commons_rect_for((0, 0), brain.phase);
        assert_eq!(commons_radius(SettlementPhase::Hamlet), 2);
        assert!(brain.commons_rect.is_some());
        let map = grass_map();
        let offsets = dense_offsets(12);
        brain.road_segments = build_road_network(&faction, &brain, &map, &offsets);
        brain.road_tiles = road_tiles_for_segments(&brain.road_segments);
        brain.road_corridor_tiles = road_corridor_tiles_for_segments(&brain.road_segments);
        use crate::economy::market::SettlementMarket;
        let settlement = Settlement {
            id: SettlementId(1),
            owner_faction: 1,
            market_tile: (0, 0),
            founding_tick: 0,
            name: "Commons".into(),
            treasury: 0.0,
            market: SettlementMarket::default(),
            peak_population: 12,
            locality: None,
        };
        let parcels = build_parcels_road_driven(&faction, &settlement, &brain, &map);
        let commons = brain.commons_rect.unwrap();
        for p in parcels.iter().filter(|p| {
            matches!(
                p.district_hint,
                Some(DistrictKind::Residential | DistrictKind::Crafting | DistrictKind::Storage)
            )
        }) {
            assert!(
                !rect_intersects_commons(Some(commons), p.rect()),
                "non-civic parcel {:?} overlaps commons {:?}",
                p.rect(),
                commons
            );
        }
    }

    #[test]
    fn commons_keepout_allows_civic_district() {
        // The commons is civic-only — `district_allowed_in_commons` /
        // `build_kind_allowed_in_commons` must permit it. Encode the policy
        // explicitly so a future change to commons rules is forced through
        // this test.
        assert!(district_allowed_in_commons(DistrictKind::Civic));
        assert!(build_kind_allowed_in_commons(OrganicBuildKind::Single(
            BuildSiteKind::Granary
        )));
        assert!(build_kind_allowed_in_commons(OrganicBuildKind::Single(
            BuildSiteKind::Well
        )));
        assert!(build_kind_allowed_in_commons(OrganicBuildKind::Single(
            BuildSiteKind::Campfire
        )));
        // And non-civic builds must be excluded.
        assert!(!build_kind_allowed_in_commons(OrganicBuildKind::Hut(
            WallMaterial::WattleDaub
        )));
        assert!(!build_kind_allowed_in_commons(OrganicBuildKind::PalisadeSegment(
            WallMaterial::WattleDaub
        )));
    }

    #[test]
    fn road_corridor_widening_matches_carver() {
        // Pure-function test: every centerline interior tile plus its
        // dominant-axis perpendicular widening tile must appear in the
        // corridor set — same rule `road_carve_system` uses to stamp.
        let segs = vec![
            StreetSegment {
                start: (-6, 0),
                end: (6, 0),
                tier: StreetTier::Primary,
            },
            StreetSegment {
                start: (0, -5),
                end: (0, 5),
                tier: StreetTier::Primary,
            },
        ];
        let corridor = road_corridor_tiles_for_segments(&segs);
        for seg in &segs {
            let (wdx, wdy) = road_widen_offset(seg.start, seg.end);
            for tile in bresenham_tiles(seg.start, seg.end) {
                if tile == seg.start || tile == seg.end {
                    continue;
                }
                assert!(
                    corridor.contains(&tile),
                    "corridor missing centerline {:?}",
                    tile
                );
                assert!(
                    corridor.contains(&(tile.0 + wdx, tile.1 + wdy)),
                    "corridor missing widening tile for {:?} (offset {:?})",
                    tile,
                    (wdx, wdy)
                );
            }
        }
    }

    #[test]
    fn parcels_respect_distance_bands_and_buffer() {
        let (faction, brain, settlement, map, _offsets) = build_village_brain((0, 0), 24);
        assert!(
            !brain.road_tiles.is_empty(),
            "village should have a road skeleton"
        );
        let parcels = build_parcels_road_driven(&faction, &settlement, &brain, &map);
        // Bands hard-reject parcels outside `[min, max]` — every emitted
        // parcel must land inside its band (band_mul > 0).
        for p in &parcels {
            let kind = p.district_hint.unwrap();
            if kind == DistrictKind::Agricultural {
                continue;
            }
            let d = cheb(p.rect().center(), (0, 0));
            let b = band_mul(kind, brain.phase, d);
            assert!(
                b > 0.0,
                "parcel {:?} kind={:?} dist={} fell outside band",
                p.rect(),
                kind,
                d
            );
        }
        // L5: no two non-ag parcels touch except via shared road frontage.
        let non_ag: Vec<&Parcel> = parcels
            .iter()
            .filter(|p| p.district_hint != Some(DistrictKind::Agricultural))
            .collect();
        for (i, a) in non_ag.iter().enumerate() {
            for b in &non_ag[i + 1..] {
                let ar = a.rect();
                let br = b.rect();
                let touches = parcels_conflict(
                    ar,
                    a.district_hint.unwrap(),
                    a.access_tile.unwrap_or((0, 0)),
                    br,
                    b.district_hint.unwrap(),
                    b.access_tile.unwrap_or((0, 0)),
                );
                assert!(
                    !touches,
                    "parcels {:?} and {:?} violate L5 spacing",
                    ar, br
                );
            }
        }
    }

    #[test]
    fn parcel_buffer_allows_shared_road_frontage() {
        // Two residentials facing each other across the same road tile must
        // not conflict — `parcels_conflict` carves out shared-frontage cases.
        let a_rect = TileRect::new(-3, 1, 3, 3); // south side of y=0 road
        let b_rect = TileRect::new(-3, -3, 3, 3); // north side of y=0 road
        let road = (-2, 0);
        assert!(!parcels_conflict(
            a_rect,
            DistrictKind::Residential,
            road,
            b_rect,
            DistrictKind::Residential,
            road,
        ));
    }

    #[test]
    fn ideal_distance_bands_separate_districts_per_phase() {
        // For Village/Chiefdom phases the per-district ideal radii must be
        // strictly ordered Storage < Residential < Crafting ≤ Sacred ≤
        // Defense — that's the donut layout the plan targets.
        for phase in [SettlementPhase::Village, SettlementPhase::Chiefdom] {
            let s = ideal_distance_band(DistrictKind::Storage, phase).unwrap().1;
            let r = ideal_distance_band(DistrictKind::Residential, phase).unwrap().1;
            let c = ideal_distance_band(DistrictKind::Crafting, phase).unwrap().1;
            let d = ideal_distance_band(DistrictKind::Defense, phase).unwrap().1;
            assert!(s <= r, "phase {:?} storage {} should be ≤ residential {}", phase, s, r);
            assert!(r <= c, "phase {:?} residential {} should be ≤ crafting {}", phase, r, c);
            assert!(c <= d, "phase {:?} crafting {} should be ≤ defense {}", phase, c, d);
        }
    }
}

// ── Bridge intent emitter ─────────────────────────────────────────────────────

/// Maximum number of consecutive River tiles a road segment may cross to
/// be eligible for bridge construction. Longer spans are rejected and the
/// planner picks an alternate route.
pub const MAX_BRIDGE_SPAN: i32 = 4;

/// Walks each settled faction's planned road segments and emits Bridge
/// blueprints for any short river crossings. Gated on:
/// - `BRIDGE_BUILDING` adopted by the community,
/// - civic-milestone threshold (`Chalcolithic, 20`) reached,
/// - the river run is `1..=MAX_BRIDGE_SPAN` tiles between two passable
///   bank tiles inside the segment.
///
/// Spawns one or more `BuildSiteKind::Bridge` blueprints (one per river
/// cell in the crossing). Each blueprint pre-computes its `work_stand`
/// at the adjacent bank so dispatchers route correctly. Skipping any
/// crossing that already has a blueprint at one of its river tiles keeps
/// emission idempotent across system ticks.
pub fn bridge_intent_emitter_system(
    clock: Res<SimClock>,
    registry: Res<FactionRegistry>,
    settlements: Query<&Settlement>,
    brains: Res<SettlementBrains>,
    chunk_map: Res<ChunkMap>,
    mut bp_map: ResMut<BlueprintMap>,
    mut commands: Commands,
) {
    if clock.tick % PRESSURE_INTERVAL != 0 {
        return;
    }
    for settlement in settlements.iter() {
        let Some(faction) = registry.factions.get(&settlement.owner_faction) else {
            continue;
        };
        if settlement.owner_faction == SOLO || !faction.caps.settlement.is_full_settlement() {
            continue;
        }
        if !faction.community_has(BRIDGE_BUILDING) {
            continue;
        }
        let era = current_era(&faction.techs);
        if !civic_milestone_allows(CivicKind::Bridge, era, settlement.peak_population) {
            continue;
        }
        let Some(brain) = brains.0.get(&settlement.id) else {
            continue;
        };
        emit_bridges_for_segments(
            settlement.owner_faction,
            &brain.road_segments,
            &chunk_map,
            &mut bp_map,
            &mut commands,
        );
    }
}

/// Pure helper: returns the river runs detected along a single Bresenham
/// trace, as `(run_start_idx, run_end_idx)` pairs, restricted to runs that
/// are `1..=MAX_BRIDGE_SPAN` and bounded on both sides by passable
/// non-water-like bank tiles. Tested independently of the spawn pipeline.
pub fn detect_bridge_runs_in_trace(
    chunk_map: &ChunkMap,
    trace: &[(i32, i32)],
) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut idx = 0;
    while idx < trace.len() {
        let (rx, ry) = trace[idx];
        let kind = match chunk_map.tile_kind_at(rx, ry) {
            Some(k) => k,
            None => {
                idx += 1;
                continue;
            }
        };
        if !matches!(kind, crate::world::tile::TileKind::River) {
            idx += 1;
            continue;
        }
        let run_start = idx;
        let mut run_end = idx;
        while run_end + 1 < trace.len() {
            let (nx, ny) = trace[run_end + 1];
            if matches!(
                chunk_map.tile_kind_at(nx, ny),
                Some(crate::world::tile::TileKind::River)
            ) {
                run_end += 1;
            } else {
                break;
            }
        }
        let run_len = (run_end - run_start + 1) as i32;
        let prev_passable = run_start > 0
            && chunk_map
                .tile_kind_at(trace[run_start - 1].0, trace[run_start - 1].1)
                .map(|k| k.is_passable() && !k.is_water_like())
                .unwrap_or(false);
        let next_passable = run_end + 1 < trace.len()
            && chunk_map
                .tile_kind_at(trace[run_end + 1].0, trace[run_end + 1].1)
                .map(|k| k.is_passable() && !k.is_water_like())
                .unwrap_or(false);
        if run_len <= MAX_BRIDGE_SPAN && prev_passable && next_passable {
            out.push((run_start, run_end));
        }
        idx = run_end + 1;
    }
    out
}

fn emit_bridges_for_segments(
    faction_id: u32,
    segments: &[StreetSegment],
    chunk_map: &ChunkMap,
    bp_map: &mut BlueprintMap,
    commands: &mut Commands,
) {
    // Cap to one new bridge blueprint per system tick per faction. Bigger
    // bursts wait for the next cadence; keeps `BlueprintMap` writes
    // bounded and prevents a long-river settlement from spawning N=20
    // bridges simultaneously.
    let mut bridges_emitted: u32 = 0;
    for seg in segments {
        if bridges_emitted >= 1 {
            return;
        }
        let trace = bresenham_tiles(seg.start, seg.end);
        let runs = detect_bridge_runs_in_trace(chunk_map, &trace);
        for (run_start, run_end) in runs {
            // Skip if any river tile in this run already has a blueprint.
            let mut already = false;
            for k in run_start..=run_end {
                if bp_map.0.contains_key(&trace[k]) {
                    already = true;
                    break;
                }
            }
            if already {
                continue;
            }
            // Spawn one Bridge blueprint at the start of the run. Multi-
            // tile crossings get re-evaluated on subsequent ticks once the
            // prior bridge finalises (its tile becomes `Bridge`, no
            // longer matching `River`).
            let tile = trace[run_start];
            let bz = chunk_map.surface_z_at(tile.0, tile.1) as i8;
            let mut bp = Blueprint::new(faction_id, None, BuildSiteKind::Bridge, tile, bz);
            bp.work_stand =
                crate::simulation::construction::work_stand_for_bridge(chunk_map, tile, bp_map);
            if bp.work_stand.is_some() {
                let wp = crate::world::terrain::tile_to_world(tile.0, tile.1);
                let bp_e = commands
                    .spawn((
                        bp,
                        Transform::from_xyz(wp.x, wp.y, 0.3),
                        GlobalTransform::default(),
                        Visibility::Visible,
                        InheritedVisibility::default(),
                    ))
                    .id();
                bp_map.0.insert(tile, bp_e);
                bridges_emitted += 1;
                if bridges_emitted >= 1 {
                    return;
                }
            }
        }
    }
}

// ── Dam intent emitter ────────────────────────────────────────────────────────

/// Chebyshev radius around the home / ag-belt reference within which the
/// emitter looks for a dammable watercourse. Settlements spawn ~13..16 tiles
/// off the river (`score_home_candidate`), so this reaches it without an
/// unbounded scan.
const DAM_SEARCH_RADIUS: i32 = 40;
/// Don't plan a dam within this chebyshev of an existing dam — one barrier
/// per reach; avoids walling an entire river.
const DAM_MIN_SPACING: i32 = 24;
/// A composite site must clear this to be worth a 6-stone/4-wood/180-work
/// commitment (keeps low-value damming out of the build queue).
const DAM_SCORE_THRESHOLD: f32 = 1.0;

/// Which need a planned dam serves (scoring provenance + diagnostics).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DamMotive {
    /// Impound upstream of the agricultural belt (water-table / dry-season
    /// resilience for the fields).
    Irrigation,
    /// Reservoir near the settlement for drinking when the river is far or
    /// runs seasonally low.
    Reservoir,
    /// A short road river-crossing where a dam (its crest carries the road)
    /// both crosses and impounds — no separate bridge needed.
    RoadCrossing,
}

/// A scored dam proposal: the watercourse `tile` to barrier and why.
#[derive(Clone, Copy, Debug)]
pub struct DamCandidate {
    pub tile: (i32, i32),
    pub score: f32,
    pub motive: DamMotive,
}

/// A River/Water tile a dam can be built on (loaded; not already a
/// Bridge/Dam).
fn is_dammable_tile(chunk_map: &ChunkMap, t: (i32, i32)) -> bool {
    matches!(
        chunk_map.tile_kind_at(t.0, t.1),
        Some(TileKind::River | TileKind::Water)
    )
}

/// Nearest dammable watercourse tile to `from`, chebyshev-ring out to
/// `max_r`. Deterministic (fixed ring order + min-tuple tiebreak). `None`
/// if none in range.
fn nearest_watercourse_tile(
    chunk_map: &ChunkMap,
    from: (i32, i32),
    max_r: i32,
) -> Option<(i32, i32)> {
    if is_dammable_tile(chunk_map, from) {
        return Some(from);
    }
    for r in 1..=max_r {
        let mut best: Option<(i32, i32)> = None;
        for dy in -r..=r {
            for dx in -r..=r {
                if dx.abs() != r && dy.abs() != r {
                    continue; // ring perimeter only
                }
                let t = (from.0 + dx, from.1 + dy);
                if is_dammable_tile(chunk_map, t) {
                    best = Some(best.map_or(t, |b| b.min(t)));
                }
            }
        }
        if best.is_some() {
            return best;
        }
    }
    None
}

/// Pure compositor: weigh the three dam motivations for one settlement and
/// return the single best proposal (argmax), or `None` if nothing clears
/// `DAM_SCORE_THRESHOLD`. `season_low ∈ [0,1]` is how far below its annual
/// mean the current discharge sits (1 = deep seasonal low) — it lifts the
/// Reservoir motive (storage matters most when the river runs thin).
fn score_dam_site(
    faction: &FactionData,
    brain: &SettlementBrain,
    chunk_map: &ChunkMap,
    season_low: f32,
) -> Option<DamCandidate> {
    let mut best: Option<DamCandidate> = None;
    let mut consider = |c: DamCandidate| {
        if c.score >= DAM_SCORE_THRESHOLD && best.map_or(true, |b| c.score > b.score) {
            best = Some(c);
        }
    };

    // ── Irrigation: dam the watercourse just upstream of the Ag belt ──
    let ag: Vec<(i32, i32)> = brain
        .parcels
        .iter()
        .filter(|p| p.district_hint == Some(DistrictKind::Agricultural))
        .map(|p| p.centre())
        .collect();
    if !ag.is_empty() {
        let cx = ag.iter().map(|t| t.0).sum::<i32>() / ag.len() as i32;
        let cy = ag.iter().map(|t| t.1).sum::<i32>() / ag.len() as i32;
        if let Some(t) = nearest_watercourse_tile(chunk_map, (cx, cy), DAM_SEARCH_RADIUS) {
            // Water must sit at/above the fields to gravity-feed them.
            let belt_g = chunk_map.ground_z_at(cx, cy);
            let water_g = chunk_map.surface_z_at(t.0, t.1);
            if water_g >= belt_g {
                let d = cheb(t, (cx, cy)) as f32;
                let prox = (1.0 - d / DAM_SEARCH_RADIUS as f32).max(0.0);
                let size = (ag.len() as f32).min(6.0) / 6.0;
                consider(DamCandidate {
                    tile: t,
                    score: 2.0 * size * (0.4 + 0.6 * prox),
                    motive: DamMotive::Irrigation,
                });
            }
        }
    }

    // ── Reservoir: water security near home when the river is far/low ──
    {
        let home = faction.home_tile;
        if let Some(t) = nearest_watercourse_tile(chunk_map, home, DAM_SEARCH_RADIUS) {
            let rd = chunk_map.river_distance_at(home.0, home.1);
            let dryness = if rd == u8::MAX {
                1.0
            } else {
                (rd as f32 / 16.0).min(1.0)
            };
            let pop = (faction.member_count as f32 / 40.0).min(1.5);
            let score = 1.5 * pop * (0.3 + 0.7 * dryness) * (1.0 + season_low);
            consider(DamCandidate {
                tile: t,
                score,
                motive: DamMotive::Reservoir,
            });
        }
    }

    // ── Road-crossing substitute: a short road run over a river ──
    for seg in &brain.road_segments {
        let trace = bresenham_tiles(seg.start, seg.end);
        for (run_start, _run_end) in detect_bridge_runs_in_trace(chunk_map, &trace) {
            consider(DamCandidate {
                tile: trace[run_start],
                score: 1.1,
                motive: DamMotive::RoadCrossing,
            });
        }
    }

    best
}

/// Walks each settled faction and, when it can build dams (`DAM_BUILDING`
/// + the `CivicKind::Dam` milestone), emits at most one `BuildSiteKind::Dam`
/// blueprint per settlement per cadence at the highest-value site from
/// `score_dam_site` (composing irrigation, reservoir water-access, and
/// road-crossing motivations). Mirrors `bridge_intent_emitter_system`
/// structurally: idempotent (skips a tile already blueprinted or near an
/// existing dam) and **author-less** (`posted_by = None`) exactly like the
/// bridge emitter — the Dam recipe has no tier picks, so finalize reads the
/// faction's live `buildable_techs`.
pub fn dam_intent_emitter_system(
    clock: Res<SimClock>,
    registry: Res<FactionRegistry>,
    settlements: Query<&Settlement>,
    brains: Res<SettlementBrains>,
    chunk_map: Res<ChunkMap>,
    calendar: Res<Calendar>,
    dam_map: Res<DamMap>,
    mut bp_map: ResMut<BlueprintMap>,
    mut commands: Commands,
) {
    if clock.tick % PRESSURE_INTERVAL != 0 {
        return;
    }
    // How far the season runs below its annual mean (mean of the four
    // multipliers ≈ 0.8625) — lifts the Reservoir motive in lean seasons.
    let season_low = ((0.8625 - calendar.discharge_multiplier()) / 0.8625).clamp(0.0, 1.0);

    for settlement in settlements.iter() {
        let Some(faction) = registry.factions.get(&settlement.owner_faction) else {
            continue;
        };
        if settlement.owner_faction == SOLO || !faction.caps.settlement.is_full_settlement() {
            continue;
        }
        if !faction.community_has(DAM_BUILDING) {
            continue;
        }
        let era = current_era(&faction.techs);
        if !civic_milestone_allows(CivicKind::Dam, era, settlement.peak_population) {
            continue;
        }
        let Some(brain) = brains.0.get(&settlement.id) else {
            continue;
        };
        let Some(cand) = score_dam_site(faction, brain, &chunk_map, season_low) else {
            continue;
        };
        let tile = cand.tile;
        // Idempotent + one-barrier-per-reach guards.
        if bp_map.0.contains_key(&tile) || !is_dammable_tile(&chunk_map, tile) {
            continue;
        }
        if dam_map.0.keys().any(|&d| cheb(d, tile) < DAM_MIN_SPACING) {
            continue;
        }
        let cz = chunk_map.surface_z_at(tile.0, tile.1) as i8;
        let mut bp = Blueprint::new(settlement.owner_faction, None, BuildSiteKind::Dam, tile, cz);
        bp.work_stand =
            crate::simulation::construction::work_stand_for_bridge(&chunk_map, tile, &bp_map);
        if bp.work_stand.is_none() {
            continue; // no reachable bank to work from — skip this reach
        }
        let wp = crate::world::terrain::tile_to_world(tile.0, tile.1);
        let e = commands
            .spawn((
                bp,
                Transform::from_xyz(wp.x, wp.y, 0.3),
                GlobalTransform::default(),
                Visibility::Visible,
                InheritedVisibility::default(),
            ))
            .id();
        bp_map.0.insert(tile, e);
    }
}

#[cfg(test)]
mod dam_emitter_tests {
    use super::*;
    use crate::world::chunk::{Chunk, ChunkCoord, ChunkMap, CHUNK_SIZE};

    /// 32×32 Grass chunk at z=4 with a vertical River line at `river_x`.
    fn chunk_with_river(river_x: usize) -> ChunkMap {
        let mut cm = ChunkMap::default();
        let z = Box::new([[4i8; CHUNK_SIZE]; CHUNK_SIZE]);
        let mut kind = Box::new([[TileKind::Grass; CHUNK_SIZE]; CHUNK_SIZE]);
        for row in kind.iter_mut() {
            row[river_x] = TileKind::River;
        }
        let fert = Box::new([[0u8; CHUNK_SIZE]; CHUNK_SIZE]);
        cm.0.insert(ChunkCoord(0, 0), Chunk::new(z, kind, fert));
        cm
    }

    #[test]
    fn nearest_watercourse_finds_the_river_deterministically() {
        let cm = chunk_with_river(20);
        // From (5,5) the closest River column is x=20 (chebyshev 15).
        let a = nearest_watercourse_tile(&cm, (5, 5), DAM_SEARCH_RADIUS);
        let b = nearest_watercourse_tile(&cm, (5, 5), DAM_SEARCH_RADIUS);
        assert_eq!(a, b, "deterministic across calls");
        let t = a.expect("a river within range");
        assert_eq!(t.0, 20, "found the river column");
        assert_eq!(cheb(t, (5, 5)), 15, "it is the nearest ring");

        // Standing on water returns the tile itself.
        assert_eq!(nearest_watercourse_tile(&cm, (20, 9), 4), Some((20, 9)));
        // No river within a tight radius from far away.
        assert_eq!(nearest_watercourse_tile(&cm, (0, 0), 3), None);
    }

    // ── Road-reservation: adaptive widen routes the corridor around structures ──

    #[test]
    fn widen_tile_prefers_default_side_when_clear() {
        // Horizontal segment widens along +Y by default.
        let t = road_widen_tile((5, 5), (0, 5), (10, 5), |_| false);
        assert_eq!(t, (5, 6));
    }

    #[test]
    fn widen_tile_flips_to_opposite_side_when_default_blocked() {
        let blocked = |p: (i32, i32)| p == (5, 6);
        let t = road_widen_tile((5, 5), (0, 5), (10, 5), blocked);
        assert_eq!(t, (5, 4), "routes around the structure to the clear side");
    }

    #[test]
    fn widen_tile_falls_back_to_default_when_both_sides_blocked() {
        let blocked = |p: (i32, i32)| p == (5, 6) || p == (5, 4);
        let t = road_widen_tile((5, 5), (0, 5), (10, 5), blocked);
        assert_eq!(t, (5, 6), "both blocked → default; carver backstop skips it");
    }

    #[test]
    fn corridor_routes_around_structure_on_default_side() {
        use crate::simulation::settlement::{StreetSegment, StreetTier};
        let segs = vec![StreetSegment {
            start: (0, 5),
            end: (4, 5),
            tier: StreetTier::Primary,
        }];
        // Structure on the default widen side of interior centerline tile (2,5).
        let blocked = |p: (i32, i32)| p == (2, 6);
        let corridor = road_corridor_tiles_for_segments_with(&segs, blocked);
        // Centerline interior tile still carves...
        assert!(corridor.contains(&(2, 5)), "centerline preserved");
        // ...but the corridor must never include the structure tile...
        assert!(
            !corridor.contains(&(2, 6)),
            "corridor must route around the structure tile"
        );
        // ...it widened to the opposite (clear) side instead.
        assert!(corridor.contains(&(2, 4)), "widened to the clear side");
    }

    #[test]
    fn corridor_baseline_is_default_two_tile_widen() {
        use crate::simulation::settlement::{StreetSegment, StreetTier};
        let segs = vec![StreetSegment {
            start: (0, 5),
            end: (4, 5),
            tier: StreetTier::Primary,
        }];
        let corridor = road_corridor_tiles_for_segments(&segs);
        // Endpoints excluded; every interior centerline tile + its +Y neighbour.
        for x in 1..=3 {
            assert!(corridor.contains(&(x, 5)), "missing centre ({x},5)");
            assert!(corridor.contains(&(x, 6)), "missing widened ({x},6)");
        }
        assert!(!corridor.contains(&(0, 5)), "start endpoint excluded");
        assert!(!corridor.contains(&(4, 5)), "end endpoint excluded");
    }
}
