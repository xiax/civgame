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

use crate::simulation::building_template::{FootprintShape, Rotation};
use crate::simulation::civic_milestones::{civic_milestone_allows, CivicKind};
use crate::simulation::construction::{
    best_wall_material, faction_can_build, find_emergency_bed_tile, recipe_for,
    select_wall_material, BarracksMap, BedMap, Blueprint, BlueprintMap, BuildSiteKind, CampfireMap,
    DamMap, DoorMap, GranaryMap, LoomMap, MarketMap, MonumentMap, RoadCarveQueue, ShrineMap,
    StructureIndex, TableMap, WallMap, WallMaterial, WallSelection, WellMap, WorkbenchMap,
    MAX_BLUEPRINTS_SAFETY_CAP,
};
use crate::simulation::faction::{FactionData, FactionMember, FactionRegistry, SOLO};
use crate::simulation::land::{tile_buildable_by, Plot, PlotIndex, TenureHolder, TileEdge};
use crate::simulation::schedule::SimClock;
use crate::simulation::settlement::{
    Settlement, SettlementId, SettlementPlan, StreetSegment, StreetSpine, StreetTier, TileRect,
    Zone, ZoneKind,
};
use crate::simulation::technology::{
    current_era, Era, BRIDGE_BUILDING, CITY_STATE_ORG, CROP_CULTIVATION, DAM_BUILDING,
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
    pub phase: SettlementPhase,
    pub anchors: Vec<SettlementAnchor>,
    pub districts: Vec<DistrictInfluence>,
    pub road_segments: Vec<StreetSegment>,
    pub road_tiles: AHashSet<(i32, i32)>,
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
            phase: SettlementPhase::Camp,
            anchors: Vec::new(),
            districts: Vec::new(),
            road_segments: Vec::new(),
            road_tiles: AHashSet::default(),
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OrganicBuildKind {
    Single(BuildSiteKind),
    Hut(WallMaterial),
    Longhouse(WallMaterial),
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

#[derive(SystemParam)]
pub struct OrganicStructureMaps<'w> {
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

/// Shared per-settlement survey body. Called by `survey_task::survey_cursor_system`
/// (one settlement per tick, paced so each settlement re-surveys at the
/// legacy `SURVEY_INTERVAL = 120` ticks cadence) and by
/// `kickoff_initial_survey_system` once at `OnEnter(GameState::Playing)` so
/// `SettlementBrain` exists *before* `seed_starting_buildings_system` picks
/// house anchors. Same effect either way — fold a fresh brain (or update
/// the existing one) into `SettlementBrains`, recompute road segments /
/// parcels / frontier, and enqueue any newly-required desire-path road
/// extensions.
///
/// Caller is expected to have already filtered out SOLO / non-settled
/// factions. Pure-function refactor (snapshot-based input → output) is
/// deferred — see `plans/evaluate-this-plan-please-eager-dolphin.md` Step
/// 2 ("Background survey infrastructure"); this helper is the bridge that
/// keeps both call sites converging on a single survey body.
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
) {
    let seed = organic_seed(settlement, faction);
    let brain = brains
        .0
        .entry(settlement.id)
        .or_insert_with(|| SettlementBrain::new(settlement.id, settlement.owner_faction, seed));

    brain.owner_faction = settlement.owner_faction;
    brain.seed = seed;
    brain.phase = phase_for(faction, settlement.peak_population);
    brain.last_survey_tick = tick;
    decay_traffic(&mut brain.traffic_heat);
    accumulate_traffic(brain, faction.home_tile, settlement.owner_faction, member_q);
    brain.anchors = collect_anchors(faction, settlement, chunk_map, maps);
    brain.districts = build_districts(faction, settlement, brain);
    let member_offsets =
        collect_member_offsets(faction.home_tile, settlement.owner_faction, member_q);
    brain.road_segments = build_road_network(faction, brain, chunk_map, &member_offsets);
    brain.road_tiles = road_tiles_for_segments(&brain.road_segments);
    brain.frontier = build_frontier(faction, brain, chunk_map, maps);
    brain.parcels = build_parcels(faction, settlement, brain, chunk_map);
    brain.layout_hash = layout_hash(faction, brain);

    maybe_queue_desire_path(
        brain,
        faction.home_tile,
        settlement.owner_faction,
        tick,
        chunk_map,
        road_queue,
    );
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
    maps: OrganicStructureMaps,
    member_q: Query<(&FactionMember, &Transform)>,
) {
    for settlement in settlements.iter() {
        let Some(faction) = registry.factions.get(&settlement.owner_faction) else {
            continue;
        };
        if settlement.owner_faction == SOLO || !faction.caps.settlement.is_full_settlement() {
            continue;
        }
        survey_one_settlement(
            settlement,
            faction,
            0,
            &mut brains,
            &mut road_queue,
            &chunk_map,
            &maps,
            &member_q,
        );
    }
    parcel_index.rebuild(&brains);
}

pub fn settlement_pressure_system(
    clock: Res<SimClock>,
    registry: Res<FactionRegistry>,
    settlements: Query<&Settlement>,
    chunk_map: Res<ChunkMap>,
    maps: OrganicStructureMaps,
    bp_map: Res<BlueprintMap>,
    bp_query: Query<&Blueprint>,
    mut pressures: ResMut<SettlementPressureMap>,
) {
    if clock.tick % PRESSURE_INTERVAL != 0 {
        return;
    }
    pressures.0.clear();

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
    maps: OrganicStructureMaps,
    bp_map: Res<BlueprintMap>,
    doormat: Res<crate::simulation::doormat::DoormatReservations>,
    archetypes: Res<BuildingArchetypeCatalog>,
) {
    if clock.tick % PRESSURE_INTERVAL != 0 {
        return;
    }
    intents.0.clear();

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
        let best = candidates
            .iter()
            .filter(|intent| {
                tile_buildable_by(&plot_index, &plot_q, intent.tile, faction_id, None)
                    && intent_tech_allowed(intent.build_kind, faction)
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

fn organic_seed(settlement: &Settlement, faction: &FactionData) -> u64 {
    (settlement.id.0 as u64)
        ^ ((faction.culture.seed as u64) << 16)
        ^ ((faction.home_tile.0 as u32 as u64) << 32)
        ^ ((faction.home_tile.1 as u32 as u64) << 1)
}

fn phase_for(faction: &FactionData, peak_population: u32) -> SettlementPhase {
    let era = current_era(&faction.techs);
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

fn accumulate_traffic(
    brain: &mut SettlementBrain,
    home: (i32, i32),
    faction_id: u32,
    member_q: &Query<(&FactionMember, &Transform)>,
) {
    let radius = survey_radius(brain.phase) + 10;
    for (member, transform) in member_q.iter() {
        if member.faction_id != faction_id {
            continue;
        }
        let tile = world_to_tile(transform.translation.truncate());
        if cheb(tile, home) > radius {
            continue;
        }
        let heat = brain.traffic_heat.entry(tile).or_insert(0);
        *heat = heat.saturating_add(18);
    }
}

fn collect_anchors(
    faction: &FactionData,
    settlement: &Settlement,
    chunk_map: &ChunkMap,
    maps: &OrganicStructureMaps,
) -> Vec<SettlementAnchor> {
    let home = faction.home_tile;
    let radius = survey_radius(phase_for(faction, settlement.peak_population));
    let mut anchors = vec![SettlementAnchor {
        kind: SettlementAnchorKind::CivicCore,
        tile: home,
        weight: 1.0,
    }];

    add_map_anchors(
        &mut anchors,
        &maps.campfire_map.0,
        home,
        32,
        SettlementAnchorKind::Hearth,
        1.0,
    );
    add_map_anchors(
        &mut anchors,
        &maps.granary_map.0,
        home,
        36,
        SettlementAnchorKind::Storehouse,
        0.9,
    );
    add_map_anchors(
        &mut anchors,
        &maps.shrine_map.0,
        home,
        36,
        SettlementAnchorKind::Shrine,
        0.75,
    );
    add_map_anchors(
        &mut anchors,
        &maps.workbench_map.0,
        home,
        36,
        SettlementAnchorKind::Workshop,
        0.7,
    );
    add_map_anchors(
        &mut anchors,
        &maps.loom_map.0,
        home,
        36,
        SettlementAnchorKind::Workshop,
        0.65,
    );
    add_map_anchors(
        &mut anchors,
        &maps.market_map.0,
        home,
        42,
        SettlementAnchorKind::Market,
        0.8,
    );
    // Built wells are first-class water anchors — orient road / parcel
    // planning around them so the village's water source lands on the
    // street network.
    add_map_anchors(
        &mut anchors,
        &maps.well_map.0,
        home,
        42,
        SettlementAnchorKind::WaterAccess,
        0.9,
    );
    add_door_gate_anchors(&mut anchors, &maps.door_map.0, home, 42);

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

fn add_map_anchors(
    anchors: &mut Vec<SettlementAnchor>,
    map: &AHashMap<(i32, i32), Entity>,
    home: (i32, i32),
    radius: i32,
    kind: SettlementAnchorKind,
    weight: f32,
) {
    for &tile in map.keys() {
        if cheb(tile, home) <= radius {
            anchors.push(SettlementAnchor { kind, tile, weight });
        }
    }
}

fn add_door_gate_anchors(
    anchors: &mut Vec<SettlementAnchor>,
    map: &AHashMap<(i32, i32), crate::simulation::construction::DoorEntry>,
    home: (i32, i32),
    radius: i32,
) {
    for &tile in map.keys() {
        if cheb(tile, home) <= radius && cheb(tile, home) >= 8 {
            anchors.push(SettlementAnchor {
                kind: SettlementAnchorKind::Gate,
                tile,
                weight: 0.55,
            });
        }
    }
}

fn build_districts(
    faction: &FactionData,
    settlement: &Settlement,
    brain: &SettlementBrain,
) -> Vec<DistrictInfluence> {
    let home = faction.home_tile;
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
                    weight: 0.6 + (faction.culture.ceremonial as f32 / 255.0) * 0.35,
                });
            }
            SettlementAnchorKind::Market | SettlementAnchorKind::Gate => {
                districts.push(DistrictInfluence {
                    kind: DistrictKind::Market,
                    centre: anchor.tile,
                    radius: 7,
                    weight: 0.6 + (faction.culture.mercantile as f32 / 255.0) * 0.35,
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
            weight: 0.45 + (faction.culture.defensive as f32 / 255.0) * 0.55,
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
pub fn primary_axis(
    faction: &FactionData,
    brain: &SettlementBrain,
    chunk_map: &ChunkMap,
    member_offsets: &[(i32, i32)],
) -> SpokeAxis {
    let home = faction.home_tile;

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

fn build_road_network(
    faction: &FactionData,
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

    let home = faction.home_tile;
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
    match brain.phase {
        SettlementPhase::Camp | SettlementPhase::Hamlet => {
            // Spine only. No grid. Lets a hamlet whose only purpose is its
            // connection to a parent city run a single spine that direction.
        }
        SettlementPhase::Village => {
            if faction.member_count >= 12 {
                push_unique_segment(&mut segments, line_through(home, perp, radius));
            }
        }
        SettlementPhase::Chiefdom => {
            for off in [-18, 18] {
                let mid = (home.0 + px * off, home.1 + py * off);
                let mut seg = line_through(mid, axis, radius);
                seg.tier = StreetTier::Secondary;
                push_unique_segment(&mut segments, seg);
            }
            let mut cross = line_through(home, perp, radius);
            cross.tier = StreetTier::Secondary;
            push_unique_segment(&mut segments, cross);
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

fn road_network_radius(phase: SettlementPhase) -> i32 {
    match phase {
        SettlementPhase::Camp => 0,
        SettlementPhase::Hamlet => 8,
        SettlementPhase::Village => 12,
        SettlementPhase::Chiefdom => 16,
        SettlementPhase::ProtoUrban => 20,
        SettlementPhase::Urban => 24,
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

fn build_frontier(
    faction: &FactionData,
    brain: &SettlementBrain,
    chunk_map: &ChunkMap,
    maps: &OrganicStructureMaps,
) -> Vec<(i32, i32)> {
    let home = faction.home_tile;
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

fn build_parcels(
    faction: &FactionData,
    settlement: &Settlement,
    brain: &SettlementBrain,
    chunk_map: &ChunkMap,
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
            faction, settlement, brain, chunk_map, &occupied, next_id, budget,
        );
        parcels.extend(belt);
        parcels
    } else {
        build_parcels_frontier_driven(faction, settlement, brain, chunk_map)
    }
}

/// Frontier-first parcel allocation. Used for camps and nomadic factions
/// that don't run the road sweep. Identical to the historical algorithm.
fn build_parcels_frontier_driven(
    faction: &FactionData,
    settlement: &Settlement,
    brain: &SettlementBrain,
    chunk_map: &ChunkMap,
) -> Vec<Parcel> {
    let mut parcels = Vec::new();
    let mut occupied: Vec<TileRect> = Vec::new();
    let mut counts: AHashMap<DistrictKind, usize> = AHashMap::default();
    let targets = parcel_targets(faction, settlement, brain.phase);
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
        if !rect_clear_for_parcel(chunk_map, rect, &brain.road_tiles) {
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

/// Road-tile-driven parcel allocation. For each road tile, derives one
/// candidate rect per cardinal × district kind, scores by
/// `suitability × deficit × proximity`, and greedily accepts non-overlapping
/// rects. Every resulting parcel has a guaranteed `frontage_edge` +
/// `access_tile` because the rect's edge is by construction adjacent to a
/// road tile.
fn build_parcels_road_driven(
    faction: &FactionData,
    settlement: &Settlement,
    brain: &SettlementBrain,
    chunk_map: &ChunkMap,
) -> Vec<Parcel> {
    let targets = parcel_targets(faction, settlement, brain.phase);
    if targets.is_empty() {
        return Vec::new();
    }

    let home = faction.home_tile;
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
                if !rect_clear_for_parcel(chunk_map, rect, &brain.road_tiles) {
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
                let score = s * (target as f32) * (1.0 / (1.0 + home_dist as f32 * 0.05));
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
    let mut occupied: Vec<TileRect> = Vec::new();
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
        if occupied.iter().any(|r| rects_overlap(*r, cand.rect)) {
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
        occupied.push(cand.rect);
        *counts.entry(cand.kind).or_insert(0) += 1;
        next_id = next_id.wrapping_add(1);
    }
    parcels
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
fn build_ag_belt(
    faction: &FactionData,
    settlement: &Settlement,
    brain: &SettlementBrain,
    chunk_map: &ChunkMap,
    occupied: &[TileRect],
    start_id: u32,
    budget: usize,
) -> Vec<Parcel> {
    const BELT_CLEARANCE: i32 = 3;
    const ACCESS_SOFT: i32 = 12;

    if budget == 0 {
        return Vec::new();
    }
    let targets = parcel_targets(faction, settlement, brain.phase);
    let ag_target = *targets.get(&DistrictKind::Agricultural).unwrap_or(&0);
    if ag_target == 0 {
        return Vec::new();
    }
    let home = faction.home_tile;

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
            if !rect_clear_for_parcel(chunk_map, rect, &brain.road_tiles) {
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
            let base_score = suitability.agricultural + road_bonus;
            let tile_hash = ((c.0 as i64).wrapping_mul(0x9E37_79B9_7F4A_7C15u64 as i64)
                ^ c.1 as i64) as u64;
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

    let cap = ag_target.min(budget);
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
                || (adj == best_key.0
                    && c.base_score == best_key.1
                    && c.tile_hash < best_key.2);
            if better {
                best = Some(i);
                best_key = (adj, c.base_score, c.tile_hash);
            }
        }
        let Some(idx) = best else { break };
        used[idx] = true;
        accepted.push(idx);
    }

    let mut parcels = Vec::with_capacity(accepted.len());
    let mut next_id = start_id;
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

fn append_pressures_for_faction(
    _faction_id: u32,
    faction: &FactionData,
    settlement: &Settlement,
    chunk_map: &ChunkMap,
    maps: &OrganicStructureMaps,
    pending: Option<&AHashMap<BuildSiteKind, u32>>,
    out: &mut Vec<SettlementPressure>,
) {
    let pending_of =
        |k: BuildSiteKind| -> u32 { pending.and_then(|p| p.get(&k).copied()).unwrap_or(0) };
    let era = current_era(&faction.techs);
    let home = faction.home_tile;
    let members = faction.member_count.max(1);
    let built_hearths = count_near(&maps.campfire_map.0, home, 32) as u32;
    let desired_hearths = match era {
        Era::Paleolithic | Era::Mesolithic => ((members + 5) / 6).max(1),
        Era::Neolithic => ((members + 7) / 8).max(1),
        _ => 1,
    };
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
            reason: "hearth coverage",
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
        && civic_milestone_allows(CivicKind::Granary, era, settlement.peak_population)
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
        && civic_milestone_allows(CivicKind::Shrine, era, settlement.peak_population)
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
        && civic_milestone_allows(CivicKind::Market, era, settlement.peak_population)
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
        && civic_milestone_allows(CivicKind::Barracks, era, settlement.peak_population)
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
        && civic_milestone_allows(CivicKind::Monument, era, settlement.peak_population)
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

fn pressure_to_intent(
    faction: &FactionData,
    brain: &SettlementBrain,
    pressure: &SettlementPressure,
    chunk_map: &ChunkMap,
    maps: &OrganicStructureMaps,
    bp_map: &BlueprintMap,
    doormat: &crate::simulation::doormat::DoormatReservations,
    archetypes: &BuildingArchetypeCatalog,
    occupied: &mut AHashSet<(i32, i32)>,
) -> Option<ConstructionIntent> {
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
            shelter_kind(era, &community_techs, pressure.population_scope, wall_mat)
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
    let tile = if shelter_emergency {
        // Era-keyed emergency annulus (deterministic via the settlement
        // layout hash + current bed count) — distinct geometry from the
        // normal residential district so it reads as outskirts/bunk/overflow
        // rows rather than proper housing.
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
        )
    }?;
    occupied.insert(tile);

    let door_dir = parcel_for_tile(brain, tile).and_then(|p| p.frontage_edge);
    Some(ConstructionIntent {
        template_id: archetype_id_for(pressure.kind, era, archetypes).to_string(),
        build_kind,
        tile,
        door_dir,
        sponsor: pressure.sponsor,
        priority: pressure.urgency + site_bonus(brain, district, tile),
        reason: pressure.reason,
    })
}

fn shelter_kind(
    era: Era,
    community_techs: &crate::simulation::faction::FactionTechs,
    bed_deficit: u32,
    wall_mat: WallMaterial,
) -> OrganicBuildKind {
    if community_techs.has(CITY_STATE_ORG)
        || (matches!(era, Era::Chalcolithic | Era::BronzeAge) && bed_deficit >= 2)
    {
        OrganicBuildKind::Longhouse(wall_mat)
    } else {
        OrganicBuildKind::Hut(wall_mat)
    }
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
) -> Option<(i32, i32)> {
    let mut candidates: Vec<(f32, (i32, i32))> = Vec::new();
    let road_frontage_required =
        faction.community_has(PERM_SETTLEMENT) && build_kind_requires_frontage(build_kind);
    for parcel in &brain.parcels {
        let tile = parcel.centre();
        if occupied.contains(&tile) {
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
        candidates.push((suitability * 100.0 + frontage_bonus + spread, tile));
    }
    for &tile in &brain.frontier {
        if road_frontage_required {
            continue;
        }
        if occupied.contains(&tile) {
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
        candidates.push((
            site_bonus(brain, district, tile) - cheb(tile, faction.home_tile) as f32 * 0.2 + spread,
            tile,
        ));
    }
    candidates.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.cmp(&b.1))
    });
    candidates.first().map(|(_, tile)| *tile)
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
            | OrganicBuildKind::Longhouse(_)
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
        OrganicBuildKind::Single(_) => {
            single_tile_clear(tile, chunk_map, maps, bp_map, doormat, brain)
        }
        OrganicBuildKind::Hut(_) => {
            footprint_clear(tile, 1, 1, chunk_map, maps, bp_map, doormat, brain)
        }
        OrganicBuildKind::Longhouse(_) => {
            footprint_clear(tile, 2, 1, chunk_map, maps, bp_map, doormat, brain)
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
        || doormat.is_reserved(tile)
        || brain.road_tiles.contains(&tile)
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
        OrganicBuildKind::Longhouse(mat) => {
            add(BuildSiteKind::Wall(mat), 8);
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
        OrganicBuildKind::Hut(mat) | OrganicBuildKind::Longhouse(mat) => {
            faction_can_build(BuildSiteKind::Wall(mat), &techs)
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
    maps: &OrganicStructureMaps,
    tile: (i32, i32),
) -> bool {
    if maps.structure_index.0.contains_key(&tile) {
        return false;
    }
    let Some(kind) = chunk_map.tile_kind_at(tile.0, tile.1) else {
        return false;
    };
    kind.is_passable() && kind != TileKind::Wall && !kind.is_water_like()
}

fn frontier_score(
    faction: &FactionData,
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
    let centre_d = cheb(tile, faction.home_tile) as f32;
    let density_pull = faction.culture.density as f32 / 255.0;
    let spacing = if centre_d < 4.0 {
        -2.0
    } else {
        (1.0 / (1.0 + centre_d * if density_pull > 0.5 { 0.08 } else { 0.03 })) * 2.0
    };
    1.0 + fertility * 2.0 + water * 1.5 + heat * 2.0 + spacing - slope_penalty
}

fn parcel_suitability(
    faction: &FactionData,
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
    let home_d = cheb(tile, faction.home_tile) as f32;
    let heat = brain.traffic_heat.get(&tile).copied().unwrap_or(0) as f32 / 255.0;
    // Terrain elevation delta (solid ground, not water surface).
    let high = (chunk_map.ground_z_at(tile.0, tile.1)
        - chunk_map.ground_z_at(faction.home_tile.0, faction.home_tile.1))
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
            + (faction.culture.ceremonial as f32 / 255.0) * 0.35
            + (1.0 / (1.0 + home_d * 0.05)) * 0.2,
        market: 0.3 + heat * 0.9 + (faction.culture.mercantile as f32 / 255.0) * 0.3,
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

fn parcel_targets(
    faction: &FactionData,
    settlement: &Settlement,
    phase: SettlementPhase,
) -> AHashMap<DistrictKind, usize> {
    let members = faction.member_count.max(settlement.peak_population).max(1) as usize;
    let era = current_era(&faction.techs);
    let mut targets = AHashMap::default();

    targets.insert(DistrictKind::Civic, 1);
    targets.insert(DistrictKind::Residential, ((members + 3) / 4).clamp(2, 24));

    if faction.community_has(CROP_CULTIVATION) {
        targets.insert(DistrictKind::Agricultural, ((members + 2) / 3).clamp(2, 24));
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
        let market_target = if faction.culture.mercantile > 180 {
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

fn maybe_queue_desire_path(
    brain: &mut SettlementBrain,
    home: (i32, i32),
    faction_id: u32,
    tick: u64,
    chunk_map: &ChunkMap,
    road_queue: &mut RoadCarveQueue,
) {
    if tick.saturating_sub(brain.last_path_carve_tick) < DESIRE_PATH_INTERVAL {
        return;
    }
    let Some((&tile, &_heat)) = brain
        .traffic_heat
        .iter()
        .filter(|(tile, heat)| **heat >= 80 && cheb(**tile, home) >= 6)
        .max_by_key(|(_, heat)| **heat)
    else {
        return;
    };
    if road_near(chunk_map, tile, 3) {
        return;
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
        return;
    }
    road_queue.0.push((faction_id, tile, home));
    brain.last_path_carve_tick = tick;
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

fn layout_hash(faction: &FactionData, brain: &SettlementBrain) -> u64 {
    let phase = brain.phase as u64;
    let pop_bucket = (faction.member_count / 5) as u64;
    let road_hash = brain
        .road_segments
        .iter()
        .fold(0u64, |acc, seg| acc ^ segment_hash(*seg));
    brain.seed
        ^ (phase << 56)
        ^ (pop_bucket << 48)
        ^ ((brain.parcels.len() as u64).min(255) << 40)
        ^ road_hash.rotate_left(7)
        ^ ((faction.culture.density as u64) << 24)
        ^ ((faction.culture.defensive as u64) << 16)
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

    /// `SettlementPhase::Village` with `member_count >= 12` must emit BOTH
    /// a primary-axis spine AND a perpendicular cross at home. Below the
    /// threshold the cross is suppressed (single-spine hamlet behaviour).
    /// Verifies the shipped phase-scaled road network.
    #[test]
    fn village_phase_emits_cross_when_population_reaches_threshold() {
        let mut faction = dummy_faction((0, 0), 12);
        force_adopt(&mut faction, PERM_SETTLEMENT);
        let mut brain = SettlementBrain::new(SettlementId(1), 1, 42);
        brain.phase = SettlementPhase::Village;
        let map = grass_map();
        let offsets: Vec<(i32, i32)> = (0..12).map(|i| (i - 6, 0)).collect();

        let segs = build_road_network(&faction, &brain, &map, &offsets);
        // With no river and a horizontal member cluster, primary axis is
        // EW. Village @ pop≥12 should have ≥2 segments (spine + cross).
        assert!(
            segs.len() >= 2,
            "village @ pop=12 expected ≥2 road segments, got {}",
            segs.len()
        );

        // Drop to 8 members — Village still (peak_population can stay high
        // but member_count gates the cross). Confirm cross is suppressed.
        faction.member_count = 8;
        let segs_small = build_road_network(&faction, &brain, &map, &offsets[..8]);
        assert_eq!(
            segs_small.len(),
            1,
            "village @ pop=8 should emit only the primary spine (no cross)"
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
        };

        let parcels = build_parcels(&faction, &settlement, &brain, &map);
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

        let parcels = build_ag_belt(&faction, &settlement, &brain, &map, &[], 100, MAX_PARCELS);
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

        let a = build_ag_belt(&faction, &settlement, &brain, &map, &[], 0, MAX_PARCELS);
        let b = build_ag_belt(&faction, &settlement, &brain, &map, &[], 0, MAX_PARCELS);
        assert!(!a.is_empty());
        let rects_a: Vec<TileRect> = a.iter().map(|p| p.rect()).collect();
        let rects_b: Vec<TileRect> = b.iter().map(|p| p.rect()).collect();
        assert_eq!(
            rects_a, rects_b,
            "build_ag_belt must be deterministic across calls"
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
        assert_eq!(ag.len(), 1, "the single belt parcel → one Agricultural zone");
        assert_eq!(ag[0].rect, belt, "ag zone uses the belt rect, not home");
    }

    /// `survey_one_settlement`'s expensive sub-functions
    /// (`build_road_network` + `build_parcels` + `road_tiles_for_segments`)
    /// are pure on `(faction, brain, chunk_map, member_offsets)`. Calling
    /// them twice with the same inputs must produce identical outputs —
    /// this is the determinism guarantee that lets the OnEnter kickoff
    /// survey and the FixedUpdate runtime survey share the same body and
    /// converge on the same brain.
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
        let mut bp = Blueprint::new(
            settlement.owner_faction,
            None,
            BuildSiteKind::Dam,
            tile,
            cz,
        );
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
}
