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
    best_wall_material, faction_can_build, recipe_for, BarracksMap, BedMap, Blueprint,
    BlueprintMap, BuildSiteKind, CampfireMap, DoorMap, GranaryMap, LoomMap, MarketMap, MonumentMap,
    RoadCarveQueue, ShrineMap, StructureIndex, TableMap, WallMap, WallMaterial, WorkbenchMap,
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
    current_era, Era, BRIDGE_BUILDING, CITY_STATE_ORG, CROP_CULTIVATION, FLINT_KNAPPING, GRANARY,
    LONG_DIST_TRADE, MONUMENTAL_BUILDING, PERM_SETTLEMENT, PROFESSIONAL_ARMY, SACRED_RITUAL,
};
use crate::simulation::terraform::PendingFootprints;
use crate::world::chunk::ChunkMap;
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
    fn new(settlement_id: SettlementId, owner_faction: u32, seed: u64) -> Self {
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
    fn rebuild(&mut self, brains: &SettlementBrains) {
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
    pub structure_index: Res<'w, StructureIndex>,
}

pub fn settlement_survey_system(
    clock: Res<SimClock>,
    mut brains: ResMut<SettlementBrains>,
    mut parcel_index: ResMut<SettlementParcelIndex>,
    mut road_queue: ResMut<RoadCarveQueue>,
    settlements: Query<&Settlement>,
    registry: Res<FactionRegistry>,
    chunk_map: Res<ChunkMap>,
    maps: OrganicStructureMaps,
    member_q: Query<(&FactionMember, &Transform)>,
) {
    if clock.tick % SURVEY_INTERVAL != 0 {
        return;
    }

    for settlement in settlements.iter() {
        let Some(faction) = registry.factions.get(&settlement.owner_faction) else {
            continue;
        };
        if settlement.owner_faction == SOLO || !faction.caps.settlement.is_full_settlement() {
            continue;
        }

        let seed = organic_seed(settlement, faction);
        let brain = brains
            .0
            .entry(settlement.id)
            .or_insert_with(|| SettlementBrain::new(settlement.id, settlement.owner_faction, seed));

        brain.owner_faction = settlement.owner_faction;
        brain.seed = seed;
        brain.phase = phase_for(faction, settlement.peak_population);
        brain.last_survey_tick = clock.tick;
        decay_traffic(&mut brain.traffic_heat);
        accumulate_traffic(
            brain,
            faction.home_tile,
            settlement.owner_faction,
            &member_q,
        );
        brain.anchors = collect_anchors(faction, settlement, &chunk_map, &maps);
        brain.districts = build_districts(faction, settlement, brain);
        brain.road_segments = build_road_network(faction, brain);
        brain.road_tiles = road_tiles_for_segments(&brain.road_segments);
        brain.frontier = build_frontier(faction, brain, &chunk_map, &maps);
        brain.parcels = build_parcels(faction, settlement, brain, &chunk_map);
        brain.layout_hash = layout_hash(faction, brain);

        maybe_queue_desire_path(
            brain,
            faction.home_tile,
            settlement.owner_faction,
            clock.tick,
            &chunk_map,
            &mut road_queue,
        );
    }

    parcel_index.rebuild(&brains);
}

pub fn settlement_pressure_system(
    clock: Res<SimClock>,
    registry: Res<FactionRegistry>,
    settlements: Query<&Settlement>,
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
    let mut by_kind: AHashMap<ZoneKind, TileRect> = AHashMap::default();
    let fallback = crate::simulation::settlement::build_settlement_plan(faction_id, faction, tick);
    for parcel in &brain.parcels {
        let Some(kind) = parcel.district_hint.map(DistrictKind::zone_kind) else {
            continue;
        };
        let rect = parcel.rect();
        by_kind
            .entry(kind)
            .and_modify(|r| *r = union_rect(*r, rect))
            .or_insert(rect);
    }
    for district in &brain.districts {
        let kind = district.kind.zone_kind();
        let r = district.radius as i32;
        let rect = TileRect::new(
            district.centre.0 - r,
            district.centre.1 - r,
            (r * 2 + 1) as u16,
            (r * 2 + 1) as u16,
        );
        by_kind
            .entry(kind)
            .and_modify(|existing| *existing = union_rect(*existing, rect))
            .or_insert(rect);
    }

    // The land-tenure layer still carves plots from SettlementPlan zones.
    // Organic parcels are authoritative for new placement, but while this
    // compatibility projection exists we preserve the old listable surfaces
    // when the organic survey has not produced that district yet.
    for legacy in fallback.zones.iter().filter(|z| {
        matches!(
            z.kind,
            ZoneKind::Residential | ZoneKind::Agricultural | ZoneKind::Crafting | ZoneKind::Storage
        )
    }) {
        by_kind.entry(legacy.kind).or_insert(legacy.rect);
    }

    let mut zones: Vec<Zone> = by_kind
        .into_iter()
        .map(|(kind, rect)| Zone {
            kind,
            rect,
            priority: zone_priority(kind, faction),
            capacity: zone_capacity(kind, faction.member_count),
            filled: 0,
        })
        .collect();
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

fn build_road_network(faction: &FactionData, brain: &SettlementBrain) -> Vec<StreetSegment> {
    if !faction.community_has(PERM_SETTLEMENT) {
        return Vec::new();
    }

    let home = faction.home_tile;
    let radius = road_network_radius(brain.phase);
    let mut segments = Vec::new();

    // A settlement's roads are the skeleton: carve the main street first, then
    // hang lots from it. Larger phases add a crossing street before organic
    // anchor paths so the core cannot be sealed by houses.
    push_unique_segment(
        &mut segments,
        StreetSegment {
            start: (home.0 - radius, home.1),
            end: (home.0 + radius, home.1),
            tier: StreetTier::Primary,
        },
    );
    if !matches!(brain.phase, SettlementPhase::Hamlet) || faction.member_count >= 10 {
        push_unique_segment(
            &mut segments,
            StreetSegment {
                start: (home.0, home.1 - radius),
                end: (home.0, home.1 + radius),
                tier: StreetTier::Primary,
            },
        );
    }

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
            endpoints.push((anchor.weight, anchor.tile, tier));
        }
    }
    for (&tile, &heat) in &brain.traffic_heat {
        if heat >= 80 && cheb(tile, home) >= 4 {
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

    if matches!(
        brain.phase,
        SettlementPhase::Chiefdom | SettlementPhase::ProtoUrban | SettlementPhase::Urban
    ) {
        let branch = (radius / 2).max(5);
        for offset in [-branch, branch] {
            push_unique_segment(
                &mut segments,
                StreetSegment {
                    start: (home.0 + offset, home.1 - branch),
                    end: (home.0 + offset, home.1 + branch),
                    tier: StreetTier::Secondary,
                },
            );
        }
    }

    segments
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

fn append_pressures_for_faction(
    _faction_id: u32,
    faction: &FactionData,
    settlement: &Settlement,
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
    let wall_mat = best_wall_material(&community_techs);
    let build_kind = match pressure.kind {
        SettlementPressureKind::Hearth => OrganicBuildKind::Single(BuildSiteKind::Campfire),
        SettlementPressureKind::Shelter if !community_techs.has(PERM_SETTLEMENT) => {
            OrganicBuildKind::Single(BuildSiteKind::Bed)
        }
        SettlementPressureKind::Shelter => {
            shelter_kind(faction, pressure.population_scope, wall_mat)
        }
        SettlementPressureKind::Storage => OrganicBuildKind::Single(BuildSiteKind::Granary),
        SettlementPressureKind::Craft => OrganicBuildKind::Single(BuildSiteKind::Workbench),
        SettlementPressureKind::Ritual => OrganicBuildKind::Single(BuildSiteKind::Shrine),
        SettlementPressureKind::Trade => OrganicBuildKind::Single(BuildSiteKind::Market),
        SettlementPressureKind::Defense => OrganicBuildKind::PalisadeSegment(wall_mat),
        SettlementPressureKind::Military => OrganicBuildKind::Single(BuildSiteKind::Barracks),
        SettlementPressureKind::Monument => OrganicBuildKind::Single(BuildSiteKind::Monument),
        SettlementPressureKind::Governance => OrganicBuildKind::Single(BuildSiteKind::Table),
        SettlementPressureKind::Field => return None,
    };
    let district = district_for_pressure(pressure.kind);
    let tile = if matches!(pressure.kind, SettlementPressureKind::Defense) {
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
    faction: &FactionData,
    bed_deficit: u32,
    wall_mat: WallMaterial,
) -> OrganicBuildKind {
    let era = current_era(&faction.techs);
    let roll_seed = faction.culture.seed ^ bed_deficit.wrapping_mul(0x9E37_79B9);
    let mut rng = fastrand::Rng::with_seed(roll_seed as u64);
    if matches!(era, Era::Chalcolithic | Era::BronzeAge) && bed_deficit <= 4 && rng.f32() < 0.12 {
        OrganicBuildKind::CompositeHouse {
            shape: FootprintShape::LShape {
                w1: 2,
                h1: 2,
                w2: 2,
                h2: 1,
            },
            rotation: Rotation::R0,
            wall_material: wall_mat,
        }
    } else if faction.community_has(CITY_STATE_ORG)
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
        let frontage_bonus = parcel.frontage_edge.map(|_| 8.0).unwrap_or(0.0);
        candidates.push((suitability * 100.0 + frontage_bonus, tile));
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
        candidates.push((
            site_bonus(brain, district, tile) - cheb(tile, faction.home_tile) as f32 * 0.2,
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
    let high = (chunk_map.surface_z_at(tile.0, tile.1)
        - chunk_map.surface_z_at(faction.home_tile.0, faction.home_tile.1))
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
        DistrictKind::Agricultural => (8, 8),
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
    let home_z = chunk_map.surface_z_at(home.0, home.1);
    let mut best: Option<(i32, i32, (i32, i32))> = None;
    for y in (home.1 - radius..=home.1 + radius).step_by(3) {
        for x in (home.0 - radius..=home.0 + radius).step_by(3) {
            let z = chunk_map.surface_z_at(x, y);
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
    let z = chunk_map.surface_z_at(tile.0, tile.1);
    [(1, 0), (-1, 0), (0, 1), (0, -1)]
        .iter()
        .map(|(dx, dy)| (chunk_map.surface_z_at(tile.0 + dx, tile.1 + dy) - z).abs())
        .max()
        .unwrap_or(0)
}

fn count_near(map: &AHashMap<(i32, i32), Entity>, home: (i32, i32), radius: i32) -> usize {
    map.keys()
        .filter(|&&tile| cheb(tile, home) <= radius)
        .count()
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
        faction.techs.unlock(tech);
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

        let segments = build_road_network(&faction, &brain);
        let tiles = road_tiles_for_segments(&segments);

        assert!(!segments.is_empty());
        assert!(tiles.contains(&(1, 0)));
        assert!(tiles.contains(&(-1, 0)));
    }

    fn flat_chunk(kind: crate::world::tile::TileKind) -> crate::world::chunk::Chunk {
        let surface_z = Box::new([[0i8; crate::world::chunk::CHUNK_SIZE]; crate::world::chunk::CHUNK_SIZE]);
        let surface_kind = Box::new([[kind; crate::world::chunk::CHUNK_SIZE]; crate::world::chunk::CHUNK_SIZE]);
        let surface_fertility = Box::new([[8u8; crate::world::chunk::CHUNK_SIZE]; crate::world::chunk::CHUNK_SIZE]);
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
    fn detect_runs_skips_run_without_two_banks() {
        let mut m = grass_map();
        write_river_at(&mut m, &[(0, 0)]);
        // Trace starts inside the river — no preceding bank tile.
        let trace: Vec<(i32, i32)> = (0..=4).map(|x| (x, 0)).collect();
        let runs = detect_bridge_runs_in_trace(&m, &trace);
        assert!(runs.is_empty());
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
