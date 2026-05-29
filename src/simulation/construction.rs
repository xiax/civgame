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
    current_era, Era, TechId, ANIMAL_HUSBANDRY, BRIDGE_BUILDING, BRONZE_CASTING, BRONZE_TOOLS,
    CITY_STATE_ORG, COPPER_TOOLS, COPPER_WORKING, DAM_BUILDING, FIRED_POTTERY, FIRE_MAKING,
    FLINT_KNAPPING, GRANARY, HORSE_TAMING, LONG_DIST_TRADE, LOOM_WEAVING, MONUMENTAL_BUILDING,
    PERM_SETTLEMENT, PORTABLE_DWELLINGS, PROFESSIONAL_ARMY, SACRED_RITUAL, TECH_TREE, WELL_DIGGING,
};
use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE, Z_MAX, Z_MIN};
use crate::world::globe::Globe;
use crate::world::seasons::{Calendar, Season};
use crate::world::terrain::{tile_to_world, TILE_SIZE};
use crate::world::tile::{TileData, TileKind};
use ahash::{AHashMap, AHashSet};
use bevy::prelude::*;
use serde::{Deserialize, Serialize};

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

/// A wall segment sitting on a tile-boundary edge (thin housing wall). Detail
/// counterpart to the cheap `world::edge::EdgeState::Wall` cache bit.
#[derive(Clone, Copy, Debug)]
pub struct EdgeWall {
    pub material: WallMaterial,
    /// `None` = natural / unowned; `Some(fid)` = constructed by faction `fid`
    /// (own-faction vision transparency, mirroring `Wall.owner_faction`).
    pub owner_faction: Option<u32>,
    /// The wall's renderable entity (durable across chunk streaming).
    pub entity: Entity,
}

/// A door sitting on a tile-boundary edge (thin housing door).
#[derive(Clone, Copy, Debug)]
pub struct EdgeDoorRef {
    pub entity: Entity,
    pub open: bool,
    pub faction_id: u32,
    pub dir: crate::simulation::land::TileEdge,
}

/// What occupies a single housing edge. A given edge is a wall, a door, or
/// nothing — never both — but both fields exist so deconstruction can clear one
/// channel without disturbing the (mutually-exclusive) other.
#[derive(Clone, Copy, Debug, Default)]
pub struct EdgeStructureEntry {
    pub wall: Option<EdgeWall>,
    pub door: Option<EdgeDoorRef>,
}

impl EdgeStructureEntry {
    /// Project to the cheap cache state read by movement/LOS hot paths.
    pub fn projected_state(&self) -> crate::world::edge::EdgeState {
        use crate::world::edge::EdgeState;
        if self.wall.is_some() {
            EdgeState::Wall
        } else if let Some(d) = self.door {
            if d.open {
                EdgeState::OpenDoor
            } else {
                EdgeState::ClosedDoor
            }
        } else {
            EdgeState::Open
        }
    }

    pub fn is_empty(&self) -> bool {
        self.wall.is_none() && self.door.is_none()
    }
}

/// Durable source of truth for housing edge walls/doors, keyed by canonical
/// `EdgeKey`. The per-chunk `ChunkEdgeBits` cache (`Chunk::edge_bits`) is a fast
/// projection of this map, re-stamped on chunk load by
/// `restamp_edge_structures_on_chunk_load` — exactly as `WallMap` ↔
/// `TileKind::Wall`. Edge entities are durable across streaming.
#[derive(Resource, Default)]
pub struct EdgeStructureMap(pub AHashMap<crate::world::edge::EdgeKey, EdgeStructureEntry>);

impl EdgeStructureMap {
    /// Owner faction of a wall on `key`, if any (for faction-aware vision LOS).
    pub fn wall_owner(&self, key: crate::world::edge::EdgeKey) -> Option<u32> {
        self.0.get(&key).and_then(|e| e.wall).and_then(|w| w.owner_faction)
    }

    /// Faction of a door on `key`, if any (own-door vision transparency).
    pub fn door_faction(&self, key: crate::world::edge::EdgeKey) -> Option<u32> {
        self.0.get(&key).and_then(|e| e.door).map(|d| d.faction_id)
    }
}

/// Stable identifier for one dwelling envelope (a hut/longhouse footprint of
/// passable floor bounded by edge walls). Monotonic; never reused.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct DwellingId(pub u32);

/// Per-dwelling metadata: the floor tiles it owns and its bounding box. The
/// whole footprint is passable floor now (walls live on edges), so placement
/// systems consult this to keep roads/plants/wells out of living space — the
/// thin-wall replacement for the old wall-tile-ring keep-out + `detect_dwellings`.
#[derive(Clone, Debug)]
pub struct DwellingInfo {
    pub tiles: Vec<(i32, i32)>,
    pub min: (i32, i32),
    pub max: (i32, i32),
}

/// Marks every floor tile that belongs to a dwelling envelope, plus per-dwelling
/// metadata. Maintained at construction/seed time alongside `EdgeStructureMap`.
#[derive(Resource, Default)]
pub struct DwellingEnvelopeMap {
    pub by_tile: AHashMap<(i32, i32), DwellingId>,
    pub dwellings: AHashMap<DwellingId, DwellingInfo>,
    next_id: u32,
}

impl DwellingEnvelopeMap {
    pub fn alloc_id(&mut self) -> DwellingId {
        let id = DwellingId(self.next_id);
        self.next_id += 1;
        id
    }

    /// Register a dwelling footprint; stamps every tile into `by_tile`.
    pub fn insert(&mut self, id: DwellingId, tiles: Vec<(i32, i32)>) {
        if tiles.is_empty() {
            return;
        }
        let mut min = tiles[0];
        let mut max = tiles[0];
        for &(x, y) in &tiles {
            min.0 = min.0.min(x);
            min.1 = min.1.min(y);
            max.0 = max.0.max(x);
            max.1 = max.1.max(y);
            self.by_tile.insert((x, y), id);
        }
        self.dwellings.insert(id, DwellingInfo { tiles, min, max });
    }

    pub fn remove(&mut self, id: DwellingId) {
        if let Some(info) = self.dwellings.remove(&id) {
            for t in info.tiles {
                if self.by_tile.get(&t) == Some(&id) {
                    self.by_tile.remove(&t);
                }
            }
        }
    }

    /// Is this tile inside any dwelling envelope (keep roads/plants/wells out)?
    pub fn contains_tile(&self, tile: (i32, i32)) -> bool {
        self.by_tile.contains_key(&tile)
    }
}

/// Marker on the durable entity that renders a thin housing wall. Phase 5
/// attaches the orientation-aware sprite; the entity exists from finalize so
/// `EdgeStructureMap` can hold a stable handle for deconstruct/replication.
#[derive(Component, Clone, Copy, Debug)]
pub struct EdgeWallVisual {
    pub material: WallMaterial,
    pub edge: crate::world::edge::EdgeKey,
}

/// Marker on the durable entity that renders a thin housing door.
#[derive(Component, Clone, Copy, Debug)]
pub struct EdgeDoorVisual {
    pub edge: crate::world::edge::EdgeKey,
    pub dir: crate::simulation::land::TileEdge,
    pub open: bool,
}

/// World-space midpoint of an edge — the render/transform anchor for an edge
/// wall/door entity (halfway between the two flanking tile centres).
pub fn edge_world_mid(edge: crate::world::edge::EdgeKey) -> Vec2 {
    let (a, b) = edge.tiles();
    let pa = tile_to_world(a.0, a.1);
    let pb = tile_to_world(b.0, b.1);
    (pa + pb) * 0.5
}

/// Semantic role of a hearth. Drives counting: `Civic` is the one public
/// fire a settled village wants near its plaza, `Domestic` are the cooking
/// fires inside individual dwellings (Longhouse interiors), `Camp` is the
/// crescent of band-camp hearths around which Paleo/Meso bedrolls cluster.
/// Intentionally has **no** `Default` impl so every spawn site is forced to
/// pick a role; silent default-to-`Camp` was the source of the original
/// Neolithic over-seeding bug.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HearthRole {
    Camp,
    Civic,
    Domestic,
}

/// Value stored in `CampfireMap`. Carries the role so pressure counting can
/// ask "how many *civic* hearths are within radius?" without scanning a
/// component query.
#[derive(Clone, Copy, Debug)]
pub struct CampfireEntry {
    pub entity: Entity,
    pub role: HearthRole,
}

/// Maps tile positions to campfire entries placed there.
#[derive(Resource, Default)]
pub struct CampfireMap(pub AHashMap<(i32, i32), CampfireEntry>);

impl CampfireMap {
    /// Entity at `tile`, role-agnostic.
    #[inline]
    pub fn entity_at(&self, tile: (i32, i32)) -> Option<Entity> {
        self.0.get(&tile).map(|e| e.entity)
    }

    /// Role of the hearth at `tile`, if any.
    #[inline]
    pub fn role_at(&self, tile: (i32, i32)) -> Option<HearthRole> {
        self.0.get(&tile).map(|e| e.role)
    }

    /// Iterate every hearth as `(tile, entity)`. Use when the caller doesn't
    /// care about role (warmth, nearest-fire lookups, despawn cleanup).
    #[inline]
    pub fn iter_any(&self) -> impl Iterator<Item = ((i32, i32), Entity)> + '_ {
        self.0.iter().map(|(t, e)| (*t, e.entity))
    }

    /// Iterate only hearths matching `role`.
    pub fn iter_role(
        &self,
        role: HearthRole,
    ) -> impl Iterator<Item = ((i32, i32), Entity)> + '_ {
        self.0
            .iter()
            .filter(move |(_, e)| e.role == role)
            .map(|(t, e)| (*t, e.entity))
    }

    /// Count hearths of `role` within chebyshev `radius` of `anchor`.
    pub fn count_role_near(
        &self,
        anchor: (i32, i32),
        radius: i32,
        role: HearthRole,
    ) -> usize {
        let (hx, hy) = anchor;
        self.0
            .iter()
            .filter(|(pos, e)| {
                e.role == role
                    && (pos.0 - hx).abs() <= radius
                    && (pos.1 - hy).abs() <= radius
            })
            .count()
    }

    /// Count hearths of any role within chebyshev `radius` of `anchor`.
    pub fn count_any_near(&self, anchor: (i32, i32), radius: i32) -> usize {
        let (hx, hy) = anchor;
        self.0
            .keys()
            .filter(|pos| (pos.0 - hx).abs() <= radius && (pos.1 - hy).abs() <= radius)
            .count()
    }
}

/// Value stored in `ShelterMap`. Carries the tier so `needs::tick_needs_system`
/// can apply the right per-day relief + radius without a component lookup.
#[derive(Clone, Copy, Debug)]
pub struct ShelterEntry {
    pub entity: Entity,
    pub tier: ShelterTier,
}

/// Tile-keyed index of lightweight shelters (`LightShelter` lean-tos, `Tent`s,
/// `Yurt`s). Populated at finalize + seed time, cleared on despawn/pack. Read
/// by `needs::tick_needs_system` to relieve `needs.shelter` for agents the
/// shelter covers. Mirrors `CampfireMap`'s warmth-lookup pattern.
#[derive(Resource, Default)]
pub struct ShelterMap(pub AHashMap<(i32, i32), ShelterEntry>);

impl ShelterMap {
    /// Strongest shelter (highest `relief_per_day`) whose `relief_radius`
    /// covers `tile`, scanning the small chebyshev neighbourhood. Returns the
    /// covering tier so the caller applies a single relief (no stacking).
    pub fn strongest_covering(&self, tile: (i32, i32)) -> Option<ShelterTier> {
        // Max shelter radius today is 1, so a 3×3 probe suffices. Kept as a
        // const so a future larger-radius shelter only changes one line.
        const MAX_SHELTER_RADIUS: i32 = 1;
        let mut best: Option<ShelterTier> = None;
        for dx in -MAX_SHELTER_RADIUS..=MAX_SHELTER_RADIUS {
            for dy in -MAX_SHELTER_RADIUS..=MAX_SHELTER_RADIUS {
                let Some(entry) = self.0.get(&(tile.0 + dx, tile.1 + dy)) else {
                    continue;
                };
                let r = entry.tier.relief_radius() as i32;
                if dx.abs().max(dy.abs()) > r {
                    continue;
                }
                if best.map_or(true, |b| entry.tier.relief_per_day() > b.relief_per_day()) {
                    best = Some(entry.tier);
                }
            }
        }
        best
    }
}

/// Maps tile positions to active Blueprint entities (faction build reservations).
#[derive(Resource, Default)]
pub struct BlueprintMap(pub AHashMap<(i32, i32), Entity>);

/// Default carved road width: centreline + one adaptive widen tile — the
/// historical 2-tile corridor every spine / desire path / building→home road
/// used before tiered widths landed.
pub const DEFAULT_ROAD_WIDTH: u8 = 2;

/// One queued road-carving job, drained by `road_carve_system` each tick.
///
/// - `Segment` is the legacy Bresenham line (spines, desire paths,
///   building→home connectors). It carves `width` tiles wide via the shared
///   `road_widen_tiles` rule (1 = centreline only, 2 = legacy corridor,
///   3 = wide artery), routing the widen side around standing structures.
/// - `Connector` carries a precomputed **cardinal** (4-connected) `path` from a
///   door's doormat to the spine. `road_carve_system` carves the path 1-wide
///   (the guaranteed continuous backbone) and only widens to `width` where the
///   perpendicular tile is free, so a door is never left diagonal-only or
///   isolated from the road graph.
#[derive(Clone, Debug)]
pub enum RoadCarveJob {
    Segment {
        faction_id: u32,
        from: (i32, i32),
        to: (i32, i32),
        width: u8,
    },
    Connector {
        faction_id: u32,
        doormat: (i32, i32),
        home: (i32, i32),
        width: u8,
    },
}

impl RoadCarveJob {
    pub fn faction_id(&self) -> u32 {
        match self {
            RoadCarveJob::Segment { faction_id, .. }
            | RoadCarveJob::Connector { faction_id, .. } => *faction_id,
        }
    }

    /// Reserve this job's footprint into `res`. `Segment` reserves its exact
    /// carved corridor; `Connector` is planned at carve time (its cardinal path
    /// isn't known until then), so it reserves a conservative 1-wide
    /// doormat→home Bresenham estimate for the build-planner guard.
    pub fn reserve_into(
        &self,
        res: &mut crate::simulation::seed_reservation::SeedReservation,
        is_blocked: impl Fn((i32, i32)) -> bool,
    ) {
        match self {
            RoadCarveJob::Segment {
                from, to, width, ..
            } => {
                crate::simulation::seed_reservation::rasterize_segment_into(
                    res, *from, *to, *width, is_blocked,
                );
            }
            RoadCarveJob::Connector { doormat, home, .. } => {
                crate::simulation::seed_reservation::rasterize_segment_into(
                    res, *doormat, *home, 1, is_blocked,
                );
            }
        }
    }
}

/// Tier- and era-aware carved width for a planned street segment. Low-tier
/// roads stay 1 tile (alleys) or the legacy 2 (secondary), while Primary
/// arteries widen to 3 tiles in the Bronze Age when settlements are large
/// enough to read as monumental thoroughfares.
pub fn road_width_for(
    tier: crate::simulation::settlement::StreetTier,
    era: crate::simulation::technology::Era,
) -> u8 {
    use crate::simulation::settlement::StreetTier;
    use crate::simulation::technology::Era;
    match tier {
        StreetTier::Alley => 1,
        StreetTier::Secondary => DEFAULT_ROAD_WIDTH,
        StreetTier::Primary => {
            if (era as u8) >= (Era::BronzeAge as u8) {
                3
            } else {
                DEFAULT_ROAD_WIDTH
            }
        }
    }
}

/// Queue of `RoadCarveJob`s populated by `construction_system` (door connectors
/// + building→home roads), the settlement planner (spine segments), and the
/// survey (desire paths). `road_carve_system` drains it each tick.
#[derive(Resource, Default)]
pub struct RoadCarveQueue(pub Vec<RoadCarveJob>);

/// Per-door tracking: stores the door entity and its current open state so
/// `has_los` can query door state by tile without joining a Bevy query.
#[derive(Clone, Copy)]
pub struct DoorEntry {
    pub entity: Entity,
    pub open: bool,
    pub faction_id: u32,
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

/// A finished physically-excavated well. The `WellMap` key is the central
/// shaft tile; drinks fire from a chebyshev-adjacent rim tile via
/// `DrinkSource::Well`. Water availability is the live `RuntimeWater` column
/// at `shaft_tile` — no virtual reach gate. See `simulation::well`.
#[derive(Component, Clone, Copy, Debug)]
pub struct Well {
    pub faction_id: u32,
    /// Central shaft tile — also the `WellMap` key and the `RuntimeWater`
    /// water-column tile the drink path reads.
    pub shaft_tile: (i32, i32),
    /// Z of the carved shaft sump (one below the water table).
    pub bottom_z: i8,
    /// Z of the original ground surface at the shaft tile. With `bottom_z`
    /// this is the durable truth for the dug stepwell geometry — read by
    /// `well::restamp_wells_on_chunk_load` to re-carve the shaft + helix
    /// after a footprint chunk streams back in.
    pub surf_z: i8,
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
    /// Read-only lightweight-shelter index, needed only to build the
    /// `organic_view` for the seed pipeline's poor-shelter placement. Mutation
    /// is owned by the `TentShelter` on_add/on_remove hooks, not this system.
    pub shelter_map: Res<'w, ShelterMap>,
    /// `WallConstructed` writer. Bundled here so `construction_system` stays
    /// under Bevy's 16-param ceiling. Lives next to `WallMap` since the same
    /// system that mutates the map fires the event.
    pub wall_constructed: bevy::ecs::event::EventWriter<'w, WallConstructed>,
    pub wall_map: ResMut<'w, WallMap>,
    /// Housing thin-wall/door source of truth. Mutated by the `EdgeWall`/
    /// `EdgeDoor` finalize arms (the dwelling-floor envelope is registered at
    /// plan time, where the full footprint is known).
    pub edge_structures: ResMut<'w, EdgeStructureMap>,
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
    /// Settlement realism: needed by the runtime door finalize so the
    /// door connector can aim at the planned spine instead of always
    /// the faction's home tile. Read-only; bundled to stay under the
    /// 16-param cap.
    pub settlement_map: Res<'w, crate::simulation::settlement::SettlementMap>,
    pub brains: Res<'w, crate::simulation::organic_settlement::SettlementBrains>,
    /// Read-only structure occupancy. The doormat carve (`write_road_tile`)
    /// consults it so a doormat never paints `Road` under a finished structure
    /// — the same anti-corruption guard `road_carve_system` carries.
    pub structure_index: Res<'w, StructureIndex>,
}

/// Bed construction tier. Tracks how the bed was built so the upgrade pipeline
/// can replace older tiers when the faction unlocks better tools.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BedTier {
    /// Woven mat / hide on bare ground — the poor-housing sleep surface. Set
    /// explicitly when a `BuildSiteKind::SleepingMat` finalises; `best_bed_for`
    /// never returns it (it is an emergency-only tier, not a tech rung). Gives
    /// `1.25×` sleep recovery in `sleep::sleep_task_system` — better than bare
    /// ground (`1.0×`), worse than any framed bed (`2.0×`).
    SleepingMat,
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
            BedTier::SleepingMat => "Sleeping Mat",
            BedTier::Crude => "Crude Bed",
            BedTier::Framed => "Framed Bed",
            BedTier::Carved => "Carved Bed",
        }
    }
    pub fn rank(self) -> u8 {
        match self {
            BedTier::SleepingMat => 0,
            BedTier::Crude => 1,
            BedTier::Framed => 2,
            BedTier::Carved => 3,
        }
    }

    /// Sleep-recovery multiplier applied in `sleep::sleep_task_system`. A
    /// sleeping mat is a partial comfort tier; every framed bed gives the full
    /// historical `2.0×`.
    pub fn sleep_recovery_mult(self) -> f32 {
        match self {
            BedTier::SleepingMat => 1.25,
            BedTier::Crude | BedTier::Framed | BedTier::Carved => 2.0,
        }
    }
}

/// Placed on completed bed entities. `owner` is the person who has claimed
/// this bed as theirs; cleared when the owner dies (`death_system`) and
/// reassigned by `assign_beds_system`. `owning_faction` is the faction that
/// built/seeded the bed (root or sub-faction id); `None` flags a legacy or
/// pre-tag spawn and is resolved at assignment time via `PlotIndex` lookup
/// then the settlement-tile-union backstop in `bed_eligible_for_faction`.
#[derive(Component, Default)]
pub struct Bed {
    pub owner: Option<Entity>,
    pub tier: BedTier,
    pub owning_faction: Option<u32>,
}

/// Persistent bed claim on a person. Inserted/updated by `assign_beds_system`.
/// `None` means the person has no claim (e.g. faction has no beds yet).
#[derive(Component, Default, Clone, Copy)]
pub struct HomeBed(pub Option<Entity>);

/// Wall construction material. Each tier requires a tech and resource mix;
/// see `BUILD_RECIPES`. All variants render as a `Wall` entity that overwrites
/// the underlying tile with `TileKind::Wall`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
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

    /// Structural-integrity ceiling — how much damage the wall absorbs before
    /// it falls. Scales with the construction tier (see
    /// `plans/vehicle-system-tanks.md` Phase 1).
    pub fn max_hp(self) -> u8 {
        match self {
            WallMaterial::Palisade => 40,
            WallMaterial::WattleDaub => 60,
            WallMaterial::Mudbrick => 90,
            WallMaterial::Stone => 140,
            WallMaterial::CutStone => 240,
        }
    }

    /// Fraction of incoming damage the wall material deflects. CutStone
    /// roughly halves an incoming hit; a Palisade absorbs everything raw.
    pub fn damage_resist(self) -> f32 {
        match self {
            WallMaterial::Palisade => 0.0,
            WallMaterial::WattleDaub => 0.10,
            WallMaterial::Mudbrick => 0.20,
            WallMaterial::Stone => 0.35,
            WallMaterial::CutStone => 0.50,
        }
    }
}

/// Marker placed on completed wall entities. `owner_faction = None` for
/// natural bedrock walls (chunk-streaming-spawned placeholders for exposed
/// rock); `Some(faction_id)` for constructed walls. The faction-aware
/// vision LOS lets observers see through their own constructed walls but
/// not natural rock or enemy walls.
#[derive(Component)]
pub struct Wall {
    pub material: WallMaterial,
    pub owner_faction: Option<u32>,
}

/// Fired when a wall is destroyed (HP reaches zero). Carries the tile so
/// pathing caches can invalidate any route that crossed it.
#[derive(Event, Clone, Copy, Debug)]
pub struct WallDestroyed {
    pub tile: (i32, i32),
}

/// Fired when a constructed wall finalises (or a natural wall first enters
/// `WallMap`). Carries the tile + owning faction so vision sources can
/// invalidate their cached visible-tile sets — see
/// `simulation::vision::recompute_dirty_vision_sets_system`. Natural walls
/// spawned by chunk streaming pass `faction = None`.
#[derive(Event, Clone, Copy, Debug)]
pub struct WallConstructed {
    pub tile: (i32, i32),
    pub faction: Option<u32>,
}

/// The single damage entry point for walls. Applies tier `damage_resist`
/// mitigation, then a saturating subtraction on `Health`. Returns `true`
/// when the hit destroys the wall (HP reaches zero).
pub fn apply_wall_damage(
    health: &mut crate::simulation::combat::Health,
    raw_damage: u8,
    material: WallMaterial,
) -> bool {
    let mitigated = (raw_damage as f32 * (1.0 - material.damage_resist())).round() as u32;
    // Sub-resist hits still chip at least 1 HP so a CutStone wall can't be
    // made effectively invulnerable by a stream of tiny hits — but only when
    // the attack actually carried damage.
    let mitigated = if raw_damage > 0 {
        mitigated.max(1)
    } else {
        0
    };
    health.current = health.current.saturating_sub(mitigated.min(255) as u8);
    health.is_dead()
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

/// Marker placed on completed campfire entities. `role` is the semantic
/// classification (see [`HearthRole`]); pressure counting and any future
/// role-specific behaviour (e.g. preferring `Civic` for hunt-muster) reads
/// it here as the durable source of truth.
#[derive(Component)]
pub struct Campfire {
    pub tier: HearthTier,
    pub role: HearthRole,
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
    /// barrier (impounded water drains). Tech-gated on the dedicated
    /// `DAM_BUILDING` (Bronze Age; prereqs `BRIDGE_BUILDING` +
    /// `MONUMENTAL_BUILDING`).
    Dam,
    /// Lined public well. 1-tile, impassable — agents drink from a
    /// chebyshev-adjacent tile via `DrinkSource::Well`. Tech-gated on
    /// `WELL_DIGGING` (Neolithic). No tile rewrite on finalize/deconstruct.
    Well,
    /// Open-air pen for housing tamed cattle/pigs near agricultural land.
    /// Tech-gated on `ANIMAL_HUSBANDRY` (Neolithic). Finalises to a `Pen`
    /// entity with default capacity 4 + species mask for Cattle/Pig.
    Pen,
    /// Roofed stable for horses. Tech-gated on `HORSE_TAMING` (Bronze).
    /// Finalises to a `Stable` entity with capacity 2 + species mask for
    /// Horse only.
    Stable,
    /// Wooden feed trough placed near a Pen or Stable. Stores grain and
    /// satisfies adjacent housed animals' hunger. Tech-gated on
    /// `ANIMAL_HUSBANDRY`.
    FeedTrough,
    /// Single hitching post — placeholder for v2 cart/plow integration.
    /// Tech-gate `ANIMAL_HUSBANDRY` (cheap, no functional payload yet).
    HitchingPost,
    /// Vehicle yard — assembly + parking anchor for the vehicle system.
    /// Finalises to a `vehicle::VehicleYard` entity. Tech-gated on
    /// `ANIMAL_HUSBANDRY`. See `plans/vehicle-system.md`.
    VehicleYard,
    /// Poor-housing sleep surface: a woven mat or hide laid on the ground.
    /// Finalises to a `Bed { tier: BedTier::SleepingMat }` so `HomeBed`
    /// assignment + the sleep dispatcher treat it like any other bed, but it
    /// only grants `1.25×` recovery. The emergency replacement for a bare
    /// `Bed` blueprint in a settled village that can't procure wall material.
    /// No tech gate. See `plans/realistic-poor-shelter.md`.
    SleepingMat(SleepingMatMaterial),
    /// Poor-housing lightweight shelter: a reed screen or thatch/brush lean-to.
    /// Finalises to a `TentShelter { tier: ShelterTier::LeanTo }` registered in
    /// `ShelterMap` + `StructureIndex` so it relieves the shelter need for
    /// agents standing under it. Not packable. No tech gate.
    LightShelter(LightShelterMaterial),
    /// Thin housing wall sitting on a tile-boundary edge (not a whole tile).
    /// The blueprint's `tile` is a flanking floor tile (build-stand anchor);
    /// the targeted edge is `Blueprint.edge`. Finalises into `EdgeStructureMap`
    /// + the per-chunk edge cache (no `TileKind::Wall` write, no `WallMap`
    /// entry). Reuses the same recipe/tier ladder as `Wall(_)`. See `world::edge`.
    EdgeWall(WallMaterial),
    /// Thin housing door sitting on a tile-boundary edge. Passable (doors never
    /// block movement) but opaque when shut. Reuses the `Door` recipe.
    EdgeDoor,
}

impl BuildSiteKind {
    /// True for housing structures that live on a tile *edge* rather than
    /// occupying a whole tile (thin walls). Their blueprint `tile` is a
    /// flanking floor anchor and `Blueprint.edge` carries the target edge.
    pub fn is_edge_structure(self) -> bool {
        matches!(self, BuildSiteKind::EdgeWall(_) | BuildSiteKind::EdgeDoor)
    }
}

/// Material a `BuildSiteKind::SleepingMat` is woven from. Drives the recipe
/// (and therefore which scarce input the chief must procure) but every variant
/// finalises to the same `BedTier::SleepingMat`. Selected by
/// `organic_settlement::select_poor_shelter_material`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SleepingMatMaterial {
    /// Absolute last resort — no materials, just a cleared patch of ground.
    BareGround,
    /// Woven from marsh reeds.
    Reed,
    /// Woven from grain-harvest thatch.
    Thatch,
    /// A laid-out animal hide.
    Hide,
}

impl SleepingMatMaterial {
    pub fn label(self) -> &'static str {
        match self {
            SleepingMatMaterial::BareGround => "Sleeping Spot",
            SleepingMatMaterial::Reed => "Reed Mat",
            SleepingMatMaterial::Thatch => "Thatch Mat",
            SleepingMatMaterial::Hide => "Hide Mat",
        }
    }
}

/// Material a `BuildSiteKind::LightShelter` is built from. Every variant
/// finalises to the same `ShelterTier::LeanTo`. Selected by
/// `organic_settlement::select_poor_shelter_material`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LightShelterMaterial {
    /// Woven reed windbreak.
    ReedScreen,
    /// Thatched lean-to on a light timber frame.
    ThatchLeanTo,
    /// Brush-and-branch lean-to.
    BrushLeanTo,
}

impl LightShelterMaterial {
    pub fn label(self) -> &'static str {
        match self {
            LightShelterMaterial::ReedScreen => "Reed Screen",
            LightShelterMaterial::ThatchLeanTo => "Thatch Lean-To",
            LightShelterMaterial::BrushLeanTo => "Brush Lean-To",
        }
    }
}

/// Marker component on tile entities representing a lightweight shelter
/// (lean-to / tent / yurt). The `on_add` / `on_remove` hooks keep `ShelterMap`
/// in sync across every spawn site (runtime finalize, seed pass, nomad camp,
/// pitch labor) and every despawn/pack — the same pattern `StructureLabel`
/// uses for `StructureIndex`. `needs::tick_needs_system` reads `ShelterMap` to
/// relieve `needs.shelter` for agents the shelter covers.
#[derive(Component, Clone, Copy, Debug)]
#[component(on_add = on_tent_shelter_add, on_remove = on_tent_shelter_remove)]
pub struct TentShelter {
    pub tier: ShelterTier,
}

fn on_tent_shelter_add(
    mut world: bevy::ecs::world::DeferredWorld<'_>,
    entity: Entity,
    _: bevy::ecs::component::ComponentId,
) {
    let Some(transform) = world.get::<Transform>(entity).copied() else {
        return;
    };
    let Some(tier) = world.get::<TentShelter>(entity).map(|s| s.tier) else {
        return;
    };
    let tile = crate::world::terrain::world_to_tile(transform.translation.truncate());
    let mut map = world.resource_mut::<ShelterMap>();
    map.0.insert(tile, ShelterEntry { entity, tier });
}

fn on_tent_shelter_remove(
    mut world: bevy::ecs::world::DeferredWorld<'_>,
    entity: Entity,
    _: bevy::ecs::component::ComponentId,
) {
    let Some(transform) = world.get::<Transform>(entity).copied() else {
        return;
    };
    let tile = crate::world::terrain::world_to_tile(transform.translation.truncate());
    let mut map = world.resource_mut::<ShelterMap>();
    if map.0.get(&tile).map(|e| e.entity) == Some(entity) {
        map.0.remove(&tile);
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ShelterTier {
    /// Reed screen / thatch / brush lean-to — the settled poor-housing shelter.
    LeanTo,
    Tent,
    Yurt,
}

impl ShelterTier {
    pub fn label(self) -> &'static str {
        match self {
            ShelterTier::LeanTo => "Lean-To",
            ShelterTier::Tent => "Tent",
            ShelterTier::Yurt => "Yurt",
        }
    }

    /// Per-game-day `needs.shelter` relief granted to an agent the shelter
    /// covers (consumed by `needs::tick_needs_system`). All tiers stay below
    /// `SHELTER_FILL_PER_SCORE_PER_DAY` (one enclosure point) so a walled
    /// house is always strictly better than the best lightweight shelter.
    pub fn relief_per_day(self) -> f32 {
        match self {
            ShelterTier::LeanTo => 20.0,
            ShelterTier::Tent => 24.0,
            ShelterTier::Yurt => 26.0,
        }
    }

    /// Chebyshev radius the shelter's relief reaches. A lean-to only shelters
    /// its own tile; tents/yurts cover one ring (you can sit just outside).
    pub fn relief_radius(self) -> u8 {
        match self {
            ShelterTier::LeanTo => 0,
            ShelterTier::Tent | ShelterTier::Yurt => 1,
        }
    }
}

impl BuildSiteKind {
    pub fn label(self) -> &'static str {
        match self {
            BuildSiteKind::Wall(mat) => mat.label(),
            BuildSiteKind::Door => "Door",
            BuildSiteKind::EdgeWall(mat) => mat.label(),
            BuildSiteKind::EdgeDoor => "Door",
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
            BuildSiteKind::Pen => "Pen",
            BuildSiteKind::Stable => "Stable",
            BuildSiteKind::FeedTrough => "Feed Trough",
            BuildSiteKind::HitchingPost => "Hitching Post",
            BuildSiteKind::VehicleYard => "Vehicle Yard",
            BuildSiteKind::SleepingMat(m) => m.label(),
            BuildSiteKind::LightShelter(m) => m.label(),
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
    /// Semantic role to stamp on the finished `Campfire`. `None` means
    /// "manual/legacy site — default to `Civic` at finalize". Only
    /// meaningful for `BuildSiteKind::Campfire`.
    pub hearth_role: Option<HearthRole>,
    /// For `EdgeWall` blueprints: bitmask (`edge_side::{N,E,S,W}`) of the
    /// outward footprint-boundary edges this perimeter tile owns — 1 bit for a
    /// mid-wall tile, 2 for a corner. `tile` is the interior floor tile (the
    /// build-stand); each set side stamps the boundary edge between `tile` and
    /// its exterior neighbour. `0` for non-edge structures. `EdgeDoor` ignores
    /// this and stamps the single edge in `door_dir`.
    pub edge_sides: u8,
}

/// Bitflags for `Blueprint.edge_sides` — which outward edges a perimeter
/// `EdgeWall` tile carries. Matches `simulation::land::TileEdge` semantics
/// (`+y` North, `+x` East).
pub mod edge_side {
    pub const N: u8 = 1;
    pub const E: u8 = 2;
    pub const S: u8 = 4;
    pub const W: u8 = 8;
}

/// Canonical `EdgeKey` for the boundary between `tile` and its neighbour one
/// step in `side`. Always orthogonal, so `EdgeKey::between` never returns None.
pub fn outward_edge_key(
    tile: (i32, i32),
    side: crate::simulation::land::TileEdge,
) -> crate::world::edge::EdgeKey {
    let (dx, dy) = side.delta();
    crate::world::edge::EdgeKey::between(tile, (tile.0 + dx, tile.1 + dy))
        .expect("cardinal neighbour is always orthogonally adjacent")
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
            hearth_role: None,
            edge_sides: 0,
        }
    }

    /// Builder: stamp the outward-edge bitmask for an `EdgeWall` perimeter tile.
    pub fn with_edge_sides(mut self, sides: u8) -> Self {
        self.edge_sides = sides;
        self
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

    /// Builder: stamp the hearth role to apply at finalize. Only meaningful
    /// for `BuildSiteKind::Campfire`; finalize falls back to
    /// `HearthRole::Civic` when this is `None` (manual right-click build).
    pub fn with_hearth_role(mut self, role: HearthRole) -> Self {
        self.hearth_role = Some(role);
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
    Pen,
    Stable,
    FeedTrough,
    HitchingPost,
    VehicleYard,
    // Poor-housing primitives — appended last so existing discriminants stay
    // stable. One recipe per material variant.
    SleepingMatBareGround,
    SleepingMatReed,
    SleepingMatThatch,
    SleepingMatHide,
    LightShelterReedScreen,
    LightShelterThatchLeanTo,
    LightShelterBrushLeanTo,
}

fn build_recipes_table() -> Vec<BuildRecipe> {
    use crate::economy::core_ids;
    let _ = core_ids::catalog();
    let wood = core_ids::wood();
    let stone = core_ids::stone();
    let skin = core_ids::skin();
    let bedroll = core_ids::bedroll();
    let packed_yurt = core_ids::packed_yurt();
    // Phase F.2 — per-technique recipe split. Each masonry tier now
    // consumes the canonical Phase F construction material that defines
    // it physically: Wattle-and-Daub binds wood lattice with reeds;
    // Mudbrick blocks are mud + straw (thatch); Cut Stone walls need
    // lime mortar. Reachability: `reeds` from Marsh tiles (Phase F.2
    // GatherReeds), `thatch` as a Grain-harvest byproduct, `lime` via
    // the FIRED_POTTERY-gated `Burn Lime` craft recipe (Limestone tiles
    // yield `limestone`). All three ingredients are reachable inside
    // the tech tier each wall recipe gates on, so the build pipeline
    // never starves on a Phase F resource the faction can't procure.
    let reeds = *core_ids::Reeds
        .get()
        .expect("core_ids: reeds not initialised");
    let thatch = *core_ids::Thatch
        .get()
        .expect("core_ids: thatch not initialised");
    let lime = *core_ids::Lime
        .get()
        .expect("core_ids: lime not initialised");

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
            inputs: vec![(wood, 2), (reeds, 1)],
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
            inputs: vec![(stone, 2), (wood, 1), (thatch, 1)],
            work_ticks: 80,
            tech_gate: Some(FIRED_POTTERY),
            deconstruct_refund: vec![(stone, 1)],
        },
        BuildRecipe {
            name: "Cut Stone Wall",
            inputs: vec![(stone, 4), (lime, 1)],
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
        // foot traffic — so more stone + longer work. Tech-gated on the
        // dedicated `DAM_BUILDING` (Bronze Age) — impounding a watershed
        // is later, larger-scale engineering than spanning one channel.
        // Player-deconstruct returns half; drop site is the nearest
        // passable bank (same as Bridge — see deconstruct path).
        BuildRecipe {
            name: "Dam",
            inputs: vec![(stone, 6), (wood, 4)],
            work_ticks: 180,
            tech_gate: Some(DAM_BUILDING),
            deconstruct_refund: vec![(stone, 3), (wood, 2)],
        },
        // Open-air pen for housing tamed cattle/pigs. Wood-fenced, stone for
        // corner posts. Tech-gated on `ANIMAL_HUSBANDRY`.
        BuildRecipe {
            name: "Pen",
            inputs: vec![(wood, 6), (stone, 2)],
            work_ticks: 80,
            tech_gate: Some(ANIMAL_HUSBANDRY),
            deconstruct_refund: vec![(wood, 3), (stone, 1)],
        },
        // Roofed stable for horses. Heavier than a pen — timber framing +
        // stone foundation. Tech-gated on `HORSE_TAMING`.
        BuildRecipe {
            name: "Stable",
            inputs: vec![(wood, 10), (stone, 4)],
            work_ticks: 140,
            tech_gate: Some(HORSE_TAMING),
            deconstruct_refund: vec![(wood, 5), (stone, 2)],
        },
        // Feed trough. Small wooden block placed near a Pen.
        BuildRecipe {
            name: "Feed Trough",
            inputs: vec![(wood, 3)],
            work_ticks: 30,
            tech_gate: Some(ANIMAL_HUSBANDRY),
            deconstruct_refund: vec![(wood, 2)],
        },
        // Hitching post — v2 cart/plow placeholder.
        BuildRecipe {
            name: "Hitching Post",
            inputs: vec![(wood, 2)],
            work_ticks: 20,
            tech_gate: Some(ANIMAL_HUSBANDRY),
            deconstruct_refund: vec![(wood, 1)],
        },
        // Vehicle yard — assembly + parking anchor. Timber-framed work area
        // with stone footings. Tech-gated on `ANIMAL_HUSBANDRY`.
        BuildRecipe {
            name: "Vehicle Yard",
            inputs: vec![(wood, 12), (stone, 6)],
            work_ticks: 120,
            tech_gate: Some(ANIMAL_HUSBANDRY),
            deconstruct_refund: vec![(wood, 6), (stone, 3)],
        },
        // --- Poor-housing primitives (see plans/realistic-poor-shelter.md) ---
        // Sleeping mats: cheap, fast, no tech. BareGround is the free last
        // resort; the woven tiers consume one cheap fibre.
        BuildRecipe {
            name: "Sleeping Spot",
            inputs: vec![],
            work_ticks: 15,
            tech_gate: None,
            deconstruct_refund: vec![],
        },
        BuildRecipe {
            name: "Reed Mat",
            inputs: vec![(reeds, 2)],
            work_ticks: 20,
            tech_gate: None,
            deconstruct_refund: vec![],
        },
        BuildRecipe {
            name: "Thatch Mat",
            inputs: vec![(thatch, 2)],
            work_ticks: 20,
            tech_gate: None,
            deconstruct_refund: vec![],
        },
        BuildRecipe {
            name: "Hide Mat",
            inputs: vec![(skin, 1)],
            work_ticks: 20,
            tech_gate: None,
            deconstruct_refund: vec![(skin, 1)],
        },
        // Lightweight shelters: a reed screen, or a thatch/brush lean-to.
        BuildRecipe {
            name: "Reed Screen",
            inputs: vec![(reeds, 3)],
            work_ticks: 40,
            tech_gate: None,
            deconstruct_refund: vec![(reeds, 1)],
        },
        BuildRecipe {
            name: "Thatch Lean-To",
            inputs: vec![(thatch, 3), (wood, 1)],
            work_ticks: 45,
            tech_gate: None,
            deconstruct_refund: vec![(wood, 1)],
        },
        BuildRecipe {
            name: "Brush Lean-To",
            inputs: vec![(wood, 2)],
            work_ticks: 40,
            tech_gate: None,
            deconstruct_refund: vec![(wood, 1)],
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

/// What poor-housing materials a settled village in emergency shelter can
/// build, picked from the cheap-fibre ladder against the same scarcity view
/// the wall selector uses. `shelter` is the lightweight-shelter material to
/// prefer (or `None` when not even brush is procurable); `mat` is the
/// sleeping-mat surface (always resolvable — `BareGround` is the free floor).
/// Returned by `select_poor_shelter_material`. See
/// `plans/realistic-poor-shelter.md`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PoorShelterSelection {
    pub shelter: Option<LightShelterMaterial>,
    pub mat: SleepingMatMaterial,
}

/// Pick poor-housing materials from the reed → thatch → brush ladder. Mirrors
/// `select_wall_material`'s scarcity walk: a `None` view (cold-start / seed)
/// treats every input as gatherable; otherwise an input is "buildable" unless
/// it is `Scarcity::Unavailable` (Available / Tight / Scarce all pass — Scarce
/// is market-procurable). Includes no pending-blueprint accounting itself; the
/// one-bp-per-window chief cadence plus the site picker's pending-aware spacing
/// keep the chief from over-queueing one scarce input.
pub fn select_poor_shelter_material(
    avail: Option<&MaterialAvailabilityView>,
) -> PoorShelterSelection {
    use crate::economy::core_ids;
    let reeds = core_ids::Reeds.get().copied();
    let thatch = core_ids::Thatch.get().copied();
    let wood = core_ids::wood();
    let skin = core_ids::skin();

    // An input is buildable unless the classifier has marked it Unavailable.
    // A `None` id (catalog not initialised) reads as not-buildable.
    let buildable = |rid: Option<crate::economy::resource_catalog::ResourceId>| -> bool {
        let Some(rid) = rid else {
            return false;
        };
        match avail {
            None => true,
            Some(v) => !matches!(v.scarcity_of(rid), Scarcity::Unavailable),
        }
    };

    let shelter = if buildable(reeds) {
        Some(LightShelterMaterial::ReedScreen)
    } else if buildable(thatch) && buildable(Some(wood)) {
        Some(LightShelterMaterial::ThatchLeanTo)
    } else if buildable(Some(wood)) {
        Some(LightShelterMaterial::BrushLeanTo)
    } else {
        None
    };

    let mat = if buildable(reeds) {
        SleepingMatMaterial::Reed
    } else if buildable(thatch) {
        SleepingMatMaterial::Thatch
    } else if buildable(Some(skin)) {
        SleepingMatMaterial::Hide
    } else {
        SleepingMatMaterial::BareGround
    };

    PoorShelterSelection { shelter, mat }
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
/// pipeline threads the era-keyed tiers into `seed_apply_intent` for
/// stamp-time tier picks; the organic pressure path reads
/// `faction.buildable_techs` (populated by the OnEnter pool refresh) for
/// intent selection. This makes the seed driver a named profile rather
/// than an ad-hoc `FactionTechs` bitset.
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

    /// The `FactionTechs` bitset for the seed pipeline. Drives tier picks
    /// in `seed_apply_intent` via the same `best_*_for` ladder the
    /// runtime chief uses.
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
            BedTier::Crude | BedTier::SleepingMat => {}
        },
        // Poor-housing primitives carry no tech gate — they are the emergency
        // fallback when no wall material is procurable.
        BuildSiteKind::SleepingMat(_) | BuildSiteKind::LightShelter(_) => {}
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
    /// candidate-*enumeration* surface — the buildable-tech bitset the
    /// organic pressure path consults via `community_has`. Actual
    /// emission is still filtered per-intent through
    /// `select_poster_for_intent`.
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
                    .count()
                    .cmp(&b.learned.count())
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
        // Thin housing walls/doors reuse the whole-tile Wall/Door recipes.
        BuildSiteKind::EdgeWall(WallMaterial::Palisade) => BuildRecipeIdx::Palisade,
        BuildSiteKind::EdgeWall(WallMaterial::WattleDaub) => BuildRecipeIdx::WattleDaub,
        BuildSiteKind::EdgeWall(WallMaterial::Stone) => BuildRecipeIdx::StoneWall,
        BuildSiteKind::EdgeWall(WallMaterial::Mudbrick) => BuildRecipeIdx::Mudbrick,
        BuildSiteKind::EdgeWall(WallMaterial::CutStone) => BuildRecipeIdx::CutStone,
        BuildSiteKind::EdgeDoor => BuildRecipeIdx::Door,
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
        BuildSiteKind::Pen => BuildRecipeIdx::Pen,
        BuildSiteKind::Stable => BuildRecipeIdx::Stable,
        BuildSiteKind::FeedTrough => BuildRecipeIdx::FeedTrough,
        BuildSiteKind::HitchingPost => BuildRecipeIdx::HitchingPost,
        BuildSiteKind::VehicleYard => BuildRecipeIdx::VehicleYard,
        BuildSiteKind::SleepingMat(SleepingMatMaterial::BareGround) => {
            BuildRecipeIdx::SleepingMatBareGround
        }
        BuildSiteKind::SleepingMat(SleepingMatMaterial::Reed) => BuildRecipeIdx::SleepingMatReed,
        BuildSiteKind::SleepingMat(SleepingMatMaterial::Thatch) => {
            BuildRecipeIdx::SleepingMatThatch
        }
        BuildSiteKind::SleepingMat(SleepingMatMaterial::Hide) => BuildRecipeIdx::SleepingMatHide,
        BuildSiteKind::LightShelter(LightShelterMaterial::ReedScreen) => {
            BuildRecipeIdx::LightShelterReedScreen
        }
        BuildSiteKind::LightShelter(LightShelterMaterial::ThatchLeanTo) => {
            BuildRecipeIdx::LightShelterThatchLeanTo
        }
        BuildSiteKind::LightShelter(LightShelterMaterial::BrushLeanTo) => {
            BuildRecipeIdx::LightShelterBrushLeanTo
        }
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
    campfire_map.count_any_near(home, radius)
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


/// Phase 1/2: plan all wall and bed blueprints for a single rectangular building.
/// The perimeter wall tile closest to camp_home becomes the entrance (left open).
/// `wall_material` controls which wall recipe is used for every perimeter tile.
/// Pick the perimeter cell on the given cardinal side of a rectangular
/// footprint. Returned as `(dx, dy)` offsets from the centre — guaranteed to
/// be a flat side (not a corner). For even-length sides the cell closest to
/// `camp_home` along the perpendicular axis is chosen so multi-building rows
/// flow naturally toward the village core.
pub(crate) fn entrance_cell_for_edge(
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
/// One tile in a walled-house plan. `door_edge` is `Some` only for the door
/// blueprint; `hearth_role` is `Some(HearthRole::Domestic)` for the
/// interior hearth a Longhouse stamps at its centre, `None` for every
/// other kind. Replaces the legacy 3-tuple so the Longhouse interior fire
/// carries its semantic role through both the immediate stamp path and
/// the deferred-terraform `PendingFootprint` path.
#[derive(Clone, Copy, Debug)]
pub struct PlannedHouseTile {
    pub kind: BuildSiteKind,
    pub tile: (i32, i32),
    pub door_edge: Option<crate::simulation::land::TileEdge>,
    pub hearth_role: Option<HearthRole>,
    /// Outward-edge bitmask for `EdgeWall` perimeter tiles (`edge_side::*`).
    /// `0` for doors / interior furniture.
    pub edge_sides: u8,
}

/// `TileEdge` → `edge_side::*` bit.
fn tile_edge_bit(e: crate::simulation::land::TileEdge) -> u8 {
    use crate::simulation::land::TileEdge as TE;
    match e {
        TE::North => edge_side::N,
        TE::East => edge_side::E,
        TE::South => edge_side::S,
        TE::West => edge_side::W,
    }
}

pub(crate) fn walled_house_tile_plan(
    cx: i32,
    cy: i32,
    half_w: i32,
    half_h: i32,
    entrance: (i32, i32),
    door_edge: crate::simulation::land::TileEdge,
    wall_material: WallMaterial,
    interior_beds: &[(i32, i32)],
    interior_hearth: Option<(i32, i32)>,
) -> Vec<PlannedHouseTile> {
    // The whole footprint is passable floor now; walls + the one door live on
    // boundary edges. One EdgeWall blueprint per perimeter tile carries that
    // tile's outward-boundary `edge_sides` (each boundary edge has exactly one
    // interior owner tile, so blueprints stay 1:1 with tiles).
    let mut plan: Vec<PlannedHouseTile> = Vec::new();
    for dy in -half_h..=half_h {
        for dx in -half_w..=half_w {
            let on_perimeter = dx.abs() == half_w || dy.abs() == half_h;
            if !on_perimeter {
                continue; // interior floor — beds + hearth via the loops below
            }
            let tile = (cx + dx, cy + dy);
            let mut sides = 0u8;
            if dx == -half_w {
                sides |= edge_side::W;
            }
            if dx == half_w {
                sides |= edge_side::E;
            }
            if dy == -half_h {
                sides |= edge_side::S;
            }
            if dy == half_h {
                sides |= edge_side::N;
            }
            if (dx, dy) == entrance {
                // Entrance tile: the door-side edge is a door. Entrances are
                // always mid-wall (one outward side), so the door is the only
                // structure on this tile. A degenerate corner entrance would
                // leave its other sides open (kept as a separate EdgeWall only
                // when non-zero, which can't collide because it's unreached in
                // practice).
                plan.push(PlannedHouseTile {
                    kind: BuildSiteKind::EdgeDoor,
                    tile,
                    door_edge: Some(door_edge),
                    hearth_role: None,
                    edge_sides: 0,
                });
                let wall_sides = sides & !tile_edge_bit(door_edge);
                if wall_sides != 0 {
                    plan.push(PlannedHouseTile {
                        kind: BuildSiteKind::EdgeWall(wall_material),
                        tile,
                        door_edge: None,
                        hearth_role: None,
                        edge_sides: wall_sides,
                    });
                }
            } else {
                plan.push(PlannedHouseTile {
                    kind: BuildSiteKind::EdgeWall(wall_material),
                    tile,
                    door_edge: None,
                    hearth_role: None,
                    edge_sides: sides,
                });
            }
        }
    }
    for &(bdx, bdy) in interior_beds {
        let tile = (cx + bdx, cy + bdy);
        plan.push(PlannedHouseTile {
            kind: BuildSiteKind::Bed,
            tile,
            door_edge: None,
            hearth_role: None,
            edge_sides: 0,
        });
    }
    if let Some((hdx, hdy)) = interior_hearth {
        let tile = (cx + hdx, cy + hdy);
        // Interior dwelling hearths are by definition Domestic — inside a
        // household's roof, not the village plaza.
        plan.push(PlannedHouseTile {
            kind: BuildSiteKind::Campfire,
            tile,
            door_edge: None,
            hearth_role: Some(HearthRole::Domestic),
            edge_sides: 0,
        });
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
    plan: &[PlannedHouseTile],
) -> bool {
    use crate::simulation::land::TileEdge as TE;
    let mut blocked_edges: AHashSet<crate::world::edge::EdgeKey> = AHashSet::new();
    let mut beds: Vec<(i32, i32)> = Vec::new();
    let mut door_interior: Option<(i32, i32)> = None;
    for entry in plan {
        match entry.kind {
            BuildSiteKind::EdgeWall(_) => {
                for (bit, side) in [
                    (edge_side::N, TE::North),
                    (edge_side::E, TE::East),
                    (edge_side::S, TE::South),
                    (edge_side::W, TE::West),
                ] {
                    if entry.edge_sides & bit != 0 {
                        blocked_edges.insert(outward_edge_key(entry.tile, side));
                    }
                }
            }
            BuildSiteKind::Bed => beds.push(entry.tile),
            BuildSiteKind::EdgeDoor => door_interior = Some(entry.tile),
            _ => {}
        }
    }
    let Some(door_interior) = door_interior else {
        return true;
    };
    crate::simulation::placement_reachability::simulate_house_reachable_edges(
        chunk_map,
        home,
        doormat,
        door_interior,
        &blocked_edges,
        &beds,
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
    interior_hearth: Option<(i32, i32)>,
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
        interior_hearth,
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
        for entry in &wall_plan {
            if bp_map.0.contains_key(&entry.tile) {
                continue;
            }
            let wp = tile_to_world(entry.tile.0 as i32, entry.tile.1 as i32);
            let mut bp = Blueprint::new(faction_id, None, entry.kind, entry.tile, target_z)
                .with_author(author);
            if let Some(e) = entry.door_edge {
                bp = bp.with_door_dir(e);
            }
            if let Some(role) = entry.hearth_role {
                bp = bp.with_hearth_role(role);
            }
            if entry.edge_sides != 0 {
                bp = bp.with_edge_sides(entry.edge_sides);
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
            bp_map.0.insert(entry.tile, e);
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
    // Road-reservation guard inputs (bundled to stay under the param ceiling):
    // `structure_index` for the adaptive widen + the carved-road check; the
    // road queue so a queued-but-uncarved corridor reserves its widened lane.
    pub structure_index: Res<'w, StructureIndex>,
    pub road_queue: Res<'w, RoadCarveQueue>,
}

impl<'w> FurnitureMaps<'w> {
    /// Borrow the read-only structure-map set as the lightweight view that
    /// `organic_settlement` helpers (`append_pressures_for_faction`,
    /// `pressure_to_intent`, …) consume. Lets the seed pipeline drive the
    /// organic intent path without going through the SystemParam `Res<T>`
    /// bundle (which can't be constructed from a `ResMut`-style
    /// `FurnitureMaps`).
    pub fn organic_view<'a>(
        &'a self,
        structure_index: &'a StructureIndex,
    ) -> crate::simulation::organic_settlement::OrganicStructureMaps<'a> {
        crate::simulation::organic_settlement::OrganicStructureMaps {
            bed_map: &*self.bed_map,
            wall_map: &*self.wall_map,
            campfire_map: &*self.campfire_map,
            shelter_map: &*self.shelter_map,
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
            structure_index,
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
    /// Hearth role to stamp on the resulting `Campfire`. `None` for
    /// non-Campfire intents (or legacy fallback paths).
    hearth_role: Option<HearthRole>,
}

#[derive(Clone, Copy)]
enum BuildIntent {
    /// Single-tile blueprint.
    Single(BuildSiteKind),
    /// 1×1 walled hut: 4 wall tiles + 1 door + 1 interior bed.
    Hut(WallMaterial),
    /// 5×3 or 3×5 walled longhouse (depending on `axis`): 8 perimeter tiles
    /// + 1 door + 2 interior beds + 1 interior hearth. `EastWest` is the
    /// legacy 5×3; `NorthSouth` rotates the long axis vertically.
    Longhouse {
        wall_material: WallMaterial,
        axis: crate::simulation::organic_settlement::HouseAxis,
    },
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
        crate::simulation::organic_settlement::OrganicBuildKind::Longhouse {
            wall_material,
            axis,
        } => BuildIntent::Longhouse {
            wall_material,
            axis,
        },
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
        hearth_role: intent.hearth_role,
    }
}

/// True if any tile of `candidate`'s footprint touches a **reserved road** —
/// the carve-faithful union of: the planned widened corridor
/// (`brain.road_corridor_tiles`, already routed around structures), a
/// queued-but-uncarved `RoadCarveQueue` segment rasterised to its full 2-tile
/// corridor, or an already-carved `TileKind::Road` tile. Replaces the old
/// centreline-only (`brain.road_tiles`) check so furniture can't land on the
/// widened lane the carver will eventually fill.
fn candidate_touches_reserved_road(
    candidate: &BuildCandidate,
    faction_id: u32,
    settlement_map: &crate::simulation::settlement::SettlementMap,
    maps: &BuildingMapsRO,
    chunk_map: &ChunkMap,
) -> bool {
    let footprint = candidate_footprint_tiles(candidate);

    // (1) Planned widened corridor for this faction's settlement.
    if let Some(sid) = settlement_map.first_for_faction(faction_id) {
        if let Some(brain) = maps.organic_brains.0.get(&sid) {
            if footprint
                .iter()
                .any(|tile| brain.road_corridor_tiles.contains(tile))
            {
                return true;
            }
        }
    }

    // (3) Already-carved roads.
    if footprint
        .iter()
        .any(|t| chunk_map.tile_kind_at(t.0, t.1) == Some(TileKind::Road))
    {
        return true;
    }

    // (2) Queued-but-uncarved corridors/connectors for this faction. Reserve
    // each queued job's full carved footprint with the same widen rule the
    // carver uses, then test the candidate footprint against it.
    let mut queued = crate::simulation::seed_reservation::SeedReservation::default();
    for job in maps.road_queue.0.iter() {
        if job.faction_id() != faction_id {
            continue;
        }
        job.reserve_into(&mut queued, |t| maps.structure_index.0.contains_key(&t));
    }
    footprint.iter().any(|t| queued.is_reserved(*t))
}

fn candidate_footprint_tiles(candidate: &BuildCandidate) -> Vec<(i32, i32)> {
    match candidate.intent {
        BuildIntent::Single(_) | BuildIntent::PalisadeSegment(_, _) => vec![candidate.tile],
        BuildIntent::Hut(_) => rect_tiles(candidate.tile, 1, 1),
        BuildIntent::Longhouse { axis, .. } => {
            let (hw, hh) = axis.longhouse_halves();
            rect_tiles(candidate.tile, hw, hh)
        }
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
            BuildIntent::Hut(mat) => {
                push(BuildSiteKind::Wall(mat));
                push(BuildSiteKind::Door);
                push(BuildSiteKind::Bed);
            }
            BuildIntent::Longhouse { wall_material, .. } => {
                push(BuildSiteKind::Wall(wall_material));
                push(BuildSiteKind::Door);
                push(BuildSiteKind::Bed);
                push(BuildSiteKind::Campfire);
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
    // Chief PersonKnowledge powers the sleepy-dove BlueprintAuthor snapshot
    // for runtime intent emission. Replaces the prior unused chief_query
    // (`AgentGoal`-gating was retired — see comment block below).
    chief_knowledge_q: Query<&crate::simulation::knowledge::PersonKnowledge, With<FactionChief>>,
    plot_index: Res<crate::simulation::land::PlotIndex>,
    plot_q: Query<&crate::simulation::land::Plot>,
    settlement_map: Res<crate::simulation::settlement::SettlementMap>,
) {
    // Phase 2.2: per-faction stagger replacing the legacy 60-tick whole-system
    // burst. Each faction is evaluated once per 60-tick window, but offsets
    // spread the work across the window. `auto_build` still hard-disables.
    if !auto_build.0 {
        return;
    }
    const SYSTEM_OFFSET: u64 = 223;
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
        if !crate::simulation::perf::faction_stagger_due(
            clock.tick,
            faction_id,
            SYSTEM_OFFSET,
            60,
        ) {
            continue;
        }
        // Household sub-factions (Market one-person + bootstrap P2 communal
        // kin groups) don't run their own construction agenda — the village
        // chief owns building. Without this gate, a household with an empty
        // `FactionTechs` (no chief entity to project from) would be treated
        // as Paleolithic by `generate_candidates` and emit Paleo crescent
        // beds when its members' bed_count drops.
        if faction.parent_faction.is_some() {
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

        // Organic pressure pipeline is the only source of construction
        // intents. `settlement_morphology_system` (Sequential, every
        // `PRESSURE_INTERVAL` ticks) writes `SelectedSettlementIntents`;
        // this chief consumes it. When no intent has been selected for a
        // faction this tick (cold start, no pressure, every candidate
        // failed parcel/commons/reachability gates), the chief simply
        // skips — the next morphology tick fills the gap. No legacy
        // `generate_candidates` fallback.
        let Some(best) = maps
            .organic_selected
            .0
            .get(&faction_id)
            .map(build_candidate_from_organic)
        else {
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
        if candidate_touches_reserved_road(&best, faction_id, &settlement_map, &maps, &chunk_map) {
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
        let settlement_id = settlement_map.first_for_faction(faction_id);
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
            best.hearth_role,
        );
    }
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
    hearth_role: Option<HearthRole>,
) {
    match intent {
        BuildIntent::Single(kind) => {
            let target_z = chunk_map.surface_z_at(tile.0 as i32, tile.1 as i32) as i8;
            let wp = tile_to_world(tile.0 as i32, tile.1 as i32);
            let mut bp =
                Blueprint::new(faction_id, None, kind, tile, target_z).with_author(author);
            if matches!(kind, BuildSiteKind::Campfire) {
                if let Some(role) = hearth_role {
                    bp = bp.with_hearth_role(role);
                }
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
            bp_map.0.insert(tile, e);
        }
        BuildIntent::Hut(wall_mat) => {
            // 3×3 hut: single interior tile holds the bed; no room for an
            // interior hearth. Huts cluster around the civic hearth instead.
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
                None,
                wall_mat,
                door_dir,
                author,
            );
        }
        BuildIntent::Longhouse {
            wall_material,
            axis,
        } => {
            // 5×3 (EastWest) or 3×5 (NorthSouth) longhouse: beds at ±1 along
            // the long axis, interior hearth at centre. Each kin-group
            // dwelling carries its own fire.
            let (half_w, half_h) = axis.longhouse_halves();
            let beds: [(i32, i32); 2] = match axis {
                crate::simulation::organic_settlement::HouseAxis::EastWest => {
                    [(-1, 0), (1, 0)]
                }
                crate::simulation::organic_settlement::HouseAxis::NorthSouth => {
                    [(0, -1), (0, 1)]
                }
            };
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
                half_w,
                half_h,
                faction_id,
                home,
                &beds,
                Some((0, 0)),
                wall_material,
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
    // Planned widened road corridor for this settlement (None for fixtures /
    // pre-survey). Seed placement must avoid it so roads stay first; the carved
    // `TileKind::Road` check below only catches roads already stamped.
    road_corridor: Option<&AHashSet<(i32, i32)>>,
) -> bool {
    if used.contains(&tile) || doormat.is_reserved(tile) {
        return false;
    }
    if road_corridor.is_some_and(|c| c.contains(&tile)) {
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
    road_corridor: Option<&AHashSet<(i32, i32)>>,
    max_radius: i32,
) -> Option<(i32, i32)> {
    for ring in 0..=max_radius {
        for dy in -ring..=ring {
            for dx in -ring..=ring {
                if dx.abs().max(dy.abs()) != ring {
                    continue;
                }
                let tile = (anchor.0 + dx, anchor.1 + dy);
                if seed_single_tile_clear(tile, used, maps, chunk_map, doormat, road_corridor) {
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
    road_corridor: Option<&AHashSet<(i32, i32)>>,
    half_w: i32,
    half_h: i32,
) -> bool {
    for dy in -half_h..=half_h {
        for dx in -half_w..=half_w {
            let tile = (anchor.0 + dx, anchor.1 + dy);
            if !seed_single_tile_clear(tile, used, maps, chunk_map, doormat, road_corridor) {
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
    interior_hearth: Option<(i32, i32)>,
    wall_material: WallMaterial,
    faction_id: u32,
    home: (i32, i32),
    door_dir: Option<crate::simulation::land::TileEdge>,
    doormat: &mut crate::simulation::doormat::DoormatReservations,
    road_carve: &mut RoadCarveQueue,
    seed_techs: &FactionTechs,
    brain: Option<&crate::simulation::organic_settlement::SettlementBrain>,
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
        interior_hearth,
        wall_material,
        faction_id,
        home,
        door_dir,
        doormat,
        road_carve,
        seed_techs,
        brain,
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
                // L1: civic commons keepout — non-civic builds may not
                // overlap the home commons disc, period. The relocate
                // spiral walks rings around the (already commons-clean)
                // anchor and can land back inside the disc if the anchor
                // sits at its edge. The organic intent layer already
                // honoured commons; this is defence in depth.
                if let Some(b) = brain {
                    let foot = crate::simulation::settlement::TileRect::new(
                        candidate.0 - half_w,
                        candidate.1 - half_h,
                        (2 * half_w + 1) as u16,
                        (2 * half_h + 1) as u16,
                    );
                    if crate::simulation::organic_settlement::rect_intersects_commons(
                        b.commons_rect,
                        foot,
                    ) {
                        continue;
                    }
                }
                if !seed_house_footprint_clear(
                    candidate,
                    used,
                    maps,
                    chunk_map,
                    doormat,
                    brain.map(|b| &b.road_corridor_tiles),
                    half_w,
                    half_h,
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
                    interior_hearth,
                    wall_material,
                    faction_id,
                    home,
                    None,
                    doormat,
                    road_carve,
                    seed_techs,
                    brain,
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
    seed_reservation: &mut crate::simulation::seed_reservation::SeedReservation,
    globe: &Globe,
    structure_index: &StructureIndex,
    faction_id: u32,
    home: (i32, i32),
    intent: BuildIntent,
    tile: (i32, i32),
    door_dir: Option<crate::simulation::land::TileEdge>,
    seed_techs: &FactionTechs,
    brain: Option<&crate::simulation::organic_settlement::SettlementBrain>,
    // Role to stamp when `intent == BuildIntent::Single(Campfire)`. Comes
    // from the originating `ConstructionIntent::hearth_role`. Other intent
    // kinds ignore this. `Civic` is the safe fallback (matches every
    // settled-Neolithic+ civic-pressure path).
    hearth_role: HearthRole,
) -> Option<(i32, i32)> {
    match intent {
        BuildIntent::Single(BuildSiteKind::Well) => {
            // Wells own a 5×5 footprint. Route through the shared seed-time
            // search so the centre lands on a 5×5-clear tile AND the aquifer
            // resolves to a buildable shaft. Original `surf - 3` constant is
            // gone — `find_seed_well_site` returns the live `WellSpec`. We
            // pass `brain: None` so the search ignores planned road tiles —
            // `road_carve_system` consults `WellMap` and detours around the
            // stamped 5×5 disc, so a footprint that overlaps a planned spine
            // is harmless once the well is in place.
            let _ = brain; // intentionally unused at seed-time well stamp
            let empty_bp_map = BlueprintMap::default();
            let empty_well_site_map = crate::simulation::well::WellSiteMap::default();
            let ctx = crate::simulation::well::WellPlacementCtx {
                structure_index,
                bp_map: &empty_bp_map,
                well_map: &maps.well_map,
                well_site_map: &empty_well_site_map,
                doormat: Some(doormat),
                seed_reservation: Some(seed_reservation),
                brain: None,
                chunk_map: Some(chunk_map),
                used: Some(used),
                self_bp: None,
            };
            // Wider search than the runtime default — at seed time the
            // dense civic build-up around home leaves many of the inner
            // tiles in `used`, and the runtime picker only re-evaluates
            // the anchor (not the disc) so we may need to walk outward
            // for an unused 5×5 site.
            let (center, spec) = crate::simulation::well::find_seed_well_site(
                tile, &ctx, globe, chunk_map, 16,
            )?;
            crate::simulation::well::stamp_seeded_well(
                commands,
                &mut maps.runtime_water,
                &mut maps.well_map,
                seed_reservation,
                tile_changed,
                used,
                faction_id,
                center,
                spec,
            );
            let _ = seed_techs;
            Some(center)
        }
        BuildIntent::Single(kind) => {
            // Seed candidate generation is intentionally shared with the
            // runtime chief and cannot see the per-faction `used` set. If the
            // selected civic anchor is the reserved home tile or another
            // freshly-stamped seed tile, nudge the single-tile structure to the
            // nearest valid neighbor instead of starving the rest of the seed
            // loop on the same high-score candidate.
            let Some(place_tile) = find_clear_seed_single_tile(
                tile,
                used,
                maps,
                chunk_map,
                doormat,
                brain.map(|b| &b.road_corridor_tiles),
                8,
            ) else {
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
                hearth_role,
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
            None,
            wall_mat,
            faction_id,
            home,
            door_dir,
            doormat,
            road_carve,
            seed_techs,
            brain,
        ),
        BuildIntent::Longhouse {
            wall_material,
            axis,
        } => {
            let (half_w, half_h) = axis.longhouse_halves();
            let beds: [(i32, i32); 2] = match axis {
                crate::simulation::organic_settlement::HouseAxis::EastWest => {
                    [(-1, 0), (1, 0)]
                }
                crate::simulation::organic_settlement::HouseAxis::NorthSouth => {
                    [(0, -1), (0, 1)]
                }
            };
            seed_walled_house_or_nearby(
                commands,
                maps,
                chunk_map,
                tile_changed,
                used,
                (tile.0, tile.1),
                half_w,
                half_h,
                &beds,
                Some((0, 0)),
                wall_material,
                faction_id,
                home,
                door_dir,
                doormat,
                road_carve,
                seed_techs,
                brain,
            )
        }
        BuildIntent::PalisadeSegment(wall_mat, _) => seed_apply_wall_tile(
            commands,
            maps,
            chunk_map,
            tile_changed,
            used,
            doormat,
            tile,
            wall_mat,
            faction_id,
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
    faction_id: u32,
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
            Wall {
                material,
                owner_faction: Some(faction_id),
            },
            crate::simulation::combat::Health::new(material.max_hp()),
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

/// Re-stamp `TileKind::Wall` for every `WallMap` tile in a freshly-loaded
/// chunk. Chunks regenerate from `Globe + seed` on stream-in, so a
/// constructed wall's tile delta is lost — only the durable `Wall` entity
/// in `WallMap` survives (the same Phase-0 gap `restamp_runtime_water` /
/// `restamp_wells` fix for Bridge/Dam/well geometry). Without this, the
/// tile under a reloaded built wall reverts to natural terrain and becomes
/// wrongly passable while the wall entity + sprite still float there.
///
/// Mirrors the Bridge/Dam `stamp` closure: skip tiles already `Wall`
/// (natural exposed bedrock regenerates as `Wall` on its own; an untouched
/// constructed wall in a resident chunk is also already `Wall`), skip
/// chunks that did not fire `ChunkLoadedEvent` this tick. Stamps one Z
/// above the regenerated surface — every construction wall path writes the
/// wall at `surface_z + 1` — and emits `TileChangedEvent` so the renderer +
/// chunk graph rebuild.
pub fn restamp_walls_on_chunk_load(
    mut events: EventReader<crate::world::chunk_streaming::ChunkLoadedEvent>,
    mut chunk_map: ResMut<ChunkMap>,
    wall_map: Res<WallMap>,
    mut tile_changed: EventWriter<crate::world::chunk_streaming::TileChangedEvent>,
) {
    let loaded: AHashSet<ChunkCoord> = events.read().map(|e| e.coord).collect();
    if loaded.is_empty() {
        return;
    }
    for &(tx, ty) in wall_map.0.keys() {
        let coord = ChunkCoord(
            tx.div_euclid(CHUNK_SIZE as i32),
            ty.div_euclid(CHUNK_SIZE as i32),
        );
        if !loaded.contains(&coord) {
            continue;
        }
        if chunk_map.tile_kind_at(tx, ty) == Some(TileKind::Wall) {
            continue;
        }
        let surf_z = chunk_map.surface_z_at(tx, ty);
        if surf_z < Z_MIN || surf_z >= Z_MAX {
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
        tile_changed.send(crate::world::chunk_streaming::TileChangedEvent { tx, ty });
    }
}

/// Re-projects housing edge structures onto freshly-streamed chunks. The
/// per-chunk `ChunkEdgeBits` cache is lost on chunk regen (like the `Wall`
/// tile delta); `EdgeStructureMap` is the durable truth. Mirrors
/// `restamp_walls_on_chunk_load`: for every edge whose owner tile lands in a
/// just-loaded chunk, re-stamp the cache state. Chained in the FixedUpdate
/// restamp batch beside the wall restamp.
pub fn restamp_edge_structures_on_chunk_load(
    mut events: EventReader<crate::world::chunk_streaming::ChunkLoadedEvent>,
    mut chunk_map: ResMut<ChunkMap>,
    edges: Res<EdgeStructureMap>,
) {
    let loaded: AHashSet<ChunkCoord> = events.read().map(|e| e.coord).collect();
    if loaded.is_empty() || edges.0.is_empty() {
        return;
    }
    for (&key, entry) in edges.0.iter() {
        let (ox, oy) = key.owner_tile();
        let coord = ChunkCoord(
            ox.div_euclid(CHUNK_SIZE as i32),
            oy.div_euclid(CHUNK_SIZE as i32),
        );
        if !loaded.contains(&coord) {
            continue;
        }
        chunk_map.set_edge_state(key, entry.projected_state());
    }
}

/// Despawns walls whose `Health` has reached zero: removes the `WallMap`
/// entry, reverts the tile to passable `Grass` (mirroring the deconstruct
/// path — `surface_z_at` reads the wall's own level), emits
/// `TileChangedEvent` so pathing caches + sprites rebuild, and fires
/// `WallDestroyed` for siege re-targeting. Runs in `Sequential` after the
/// damage systems (`vehicle_siege_system`, `projectile_system`).
pub fn wall_destruction_system(
    mut commands: Commands,
    mut wall_map: ResMut<WallMap>,
    mut chunk_map: ResMut<ChunkMap>,
    health_q: Query<&crate::simulation::combat::Health, With<Wall>>,
    mut tile_changed: EventWriter<crate::world::chunk_streaming::TileChangedEvent>,
    mut destroyed: EventWriter<WallDestroyed>,
) {
    let dead: Vec<((i32, i32), Entity)> = wall_map
        .0
        .iter()
        .filter(|(_, &e)| health_q.get(e).map(|h| h.is_dead()).unwrap_or(false))
        .map(|(&t, &e)| (t, e))
        .collect();
    for (tile, entity) in dead {
        wall_map.0.remove(&tile);
        let surf_z = chunk_map.surface_z_at(tile.0, tile.1);
        chunk_map.set_tile(
            tile.0,
            tile.1,
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
        destroyed.send(WallDestroyed { tile });
        commands.entity(entity).despawn_recursive();
    }
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
    // Build a `RoadField` from carved roads only (no planned-spine info
    // available at this layer). Cheap — walks road tiles once.
    let empty_planned: ahash::AHashSet<(i32, i32)> = ahash::AHashSet::new();
    let road_field = crate::simulation::placement_reachability::road_field_from_home(
        chunk_map,
        &empty_planned,
        home,
    );
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
        // Rank by `total_steps` (off-road + on-road) when the road graph
        // exists. Fall back to squared-distance when no road has been
        // carved yet (very first seed ticks).
        let d = if road_field.home_road_tile.is_some() {
            use crate::simulation::placement_reachability as reach;
            reach::path_stats(chunk_map, &road_field, reach::resolve3(chunk_map, dm), home)
                .map(|s| s.total_steps as i64)
                .unwrap_or_else(|| {
                    ((dm.0 - home.0) as i64).pow(2) + ((dm.1 - home.1) as i64).pow(2)
                })
        } else {
            ((dm.0 - home.0) as i64).pow(2) + ((dm.1 - home.1) as i64).pow(2)
        };
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

/// Settlement realism: how should a freshly-stamped door's connector reach the
/// road graph? Each door places one `Road` doormat tile. The connector must be
/// **cardinally** (4-connected) continuous into the spine — a diagonal-only
/// touch reads as a broken road and leaves an unpaved strip that grows weeds.
/// `find_door_connector` runs one bounded cardinal search that returns the
/// actual carvable path, so carving every tile guarantees an unbroken road
/// that routes *around* structures and farm tiles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DoorConnectorPlan {
    /// The doormat already shares a cardinal edge with a road tile on the
    /// home-connected graph — no extension carve needed.
    AlreadyConnected,
    /// Carve these cardinally-chained, already-carvable tiles (doormat-
    /// exclusive). The last tile is cardinally adjacent to the spine, so the
    /// carved result joins the road graph with no diagonal break or gap.
    Connector(Vec<(i32, i32)>),
    /// No spine reachable within radius. The cardinal path toward `home_tile`
    /// (carvable tiles only), or empty when even that is boxed in — callers may
    /// then fall back to a legacy Bresenham `Segment` toward home.
    HomeFallback(Vec<(i32, i32)>),
}

/// True iff `road_carve_system` would flip `tile` to `Road` — i.e. the cardinal
/// connector search may step here. Mirrors `try_write_road`'s tile-kind gate
/// (Cropland is explicitly rejected before the soil-like check, so tilled farm
/// land is avoided for free); `blocked` layers structure/bed/well/blueprint and
/// planned-farm avoidance on top.
fn connector_tile_carvable(
    chunk_map: &ChunkMap,
    tile: (i32, i32),
    blocked: &impl Fn((i32, i32)) -> bool,
) -> bool {
    if blocked(tile) {
        return false;
    }
    matches!(
        chunk_map.tile_kind_at(tile.0, tile.1),
        Some(TileKind::Grass) | Some(TileKind::Scrub) | Some(TileKind::Sand)
    ) || chunk_map
        .tile_kind_at(tile.0, tile.1)
        .map(|k| k.is_soil_like() && k != TileKind::Cropland)
        .unwrap_or(false)
}

const CONNECTOR_MAX_EXPANSIONS: usize = 1024;
const CARDINAL_DIRS: [(i32, i32); 4] = [(1, 0), (-1, 0), (0, 1), (0, -1)];

/// Bounded 4-connected BFS from `doormat` over carvable tiles, terminating at
/// the first carvable tile cardinally adjacent to a `is_target` tile. Returns
/// the path (doormat-exclusive) of carvable, cardinally-chained tiles, or
/// `None` if no target is reachable within `radius` / `CONNECTOR_MAX_EXPANSIONS`.
/// BFS gives the shortest off-road connector.
fn cardinal_path_to_target(
    chunk_map: &ChunkMap,
    doormat: (i32, i32),
    radius: i32,
    blocked: &impl Fn((i32, i32)) -> bool,
    is_target: &impl Fn((i32, i32)) -> bool,
) -> Option<Vec<(i32, i32)>> {
    use std::collections::VecDeque;
    let mut came_from: ahash::AHashMap<(i32, i32), (i32, i32)> = ahash::AHashMap::new();
    let mut visited: ahash::AHashSet<(i32, i32)> = ahash::AHashSet::new();
    let mut queue: VecDeque<(i32, i32)> = VecDeque::new();
    visited.insert(doormat);
    queue.push_back(doormat);
    let mut expansions = 0usize;
    while let Some(cur) = queue.pop_front() {
        expansions += 1;
        if expansions > CONNECTOR_MAX_EXPANSIONS {
            break;
        }
        for (dx, dy) in CARDINAL_DIRS {
            let nbr = (cur.0 + dx, cur.1 + dy);
            if visited.contains(&nbr) {
                continue;
            }
            if (nbr.0 - doormat.0).abs().max((nbr.1 - doormat.1).abs()) > radius {
                continue;
            }
            // Only step onto carvable tiles. (Targets that are already Road are
            // not carvable, so we never step *onto* the spine — we stop one
            // tile short, cardinally adjacent to it.)
            if !connector_tile_carvable(chunk_map, nbr, blocked) {
                continue;
            }
            visited.insert(nbr);
            came_from.insert(nbr, cur);
            // Reached a carvable tile that touches the spine cardinally → done.
            if CARDINAL_DIRS
                .iter()
                .any(|(tx, ty)| is_target((nbr.0 + tx, nbr.1 + ty)))
            {
                let mut path = vec![nbr];
                let mut t = nbr;
                while let Some(&p) = came_from.get(&t) {
                    if p == doormat {
                        break;
                    }
                    path.push(p);
                    t = p;
                }
                path.reverse();
                return Some(path);
            }
            queue.push_back(nbr);
        }
    }
    None
}

/// A tile sits inside some well's 5×5 stepwell footprint iff its chebyshev
/// distance to any registered well centre is ≤ 2. Shared by the connector
/// search's `blocked` predicate and `road_carve_system`'s well detour.
pub(crate) fn tile_in_well_footprint(well_map: &WellMap, tile: (i32, i32)) -> bool {
    well_map
        .0
        .keys()
        .any(|c| (c.0 - tile.0).abs() <= 2 && (c.1 - tile.1).abs() <= 2)
}

/// Queue a door connector. The cardinal path is *planned at carve time* by
/// `road_carve_system` (which holds the final structure + farm maps and the
/// settlement brain), so a later-seeded house can't invalidate a path computed
/// early. The queue carries only the doormat + home endpoints.
fn queue_door_connector(
    road_queue: &mut RoadCarveQueue,
    faction_id: u32,
    doormat: (i32, i32),
    home_tile: (i32, i32),
) {
    road_queue.0.push(RoadCarveJob::Connector {
        faction_id,
        doormat,
        home: home_tile,
        width: 1,
    });
}

/// Plan how a fresh door at `doormat` connects to the settlement road graph.
///
/// Runs a single bounded **cardinal** (4-connected) search over carvable tiles:
/// - `AlreadyConnected` when the doormat already shares a cardinal edge with a
///   home-connected road / planned-spine tile.
/// - `Connector(path)` — the shortest carvable cardinal path ending one tile
///   short of the spine. Every tile is carvable, so carving the whole path
///   yields an unbroken road into the graph.
/// - `HomeFallback(path)` when no spine is reachable within `radius`: a cardinal
///   path toward `home_tile`, or empty if even that is boxed in.
///
/// `blocked(tile)` must report every tile the carver would refuse (finished
/// structures, beds, wells, blueprints) so the returned path is guaranteed
/// carvable. Planned Agricultural parcels (kitchen gardens) and tilled
/// `Cropland` are avoided automatically — the search routes *around* farms.
/// `radius` defaults to 12 in callers.
pub fn find_door_connector(
    chunk_map: &ChunkMap,
    brain: Option<&crate::simulation::organic_settlement::SettlementBrain>,
    home_tile: (i32, i32),
    doormat: (i32, i32),
    radius: i32,
    blocked: impl Fn((i32, i32)) -> bool,
) -> DoorConnectorPlan {
    // Farm avoidance: never route a connector through a planned Agricultural
    // parcel (kitchen garden) even before it's tilled. Tilled `Cropland` is
    // already excluded by `connector_tile_carvable`.
    let in_ag_parcel = |tile: (i32, i32)| -> bool {
        brain
            .map(|b| {
                b.parcels
                    .iter()
                    .filter(|p| {
                        p.district_hint
                            == Some(crate::simulation::organic_settlement::DistrictKind::Agricultural)
                    })
                    .any(|p| p.rect().contains(tile.0, tile.1))
            })
            .unwrap_or(false)
    };
    let step_blocked = |tile: (i32, i32)| blocked(tile) || in_ag_parcel(tile);

    // A tile is a connection target if it's a carved Road (≠ the doormat we just
    // stamped) or a planned spine tile (centreline ∪ widened corridor). Both
    // get carved before/with this connector, so terminating cardinally adjacent
    // to one yields a continuous join.
    let is_spine = |tile: (i32, i32)| -> bool {
        if tile == doormat {
            return false;
        }
        if chunk_map.tile_kind_at(tile.0, tile.1) == Some(TileKind::Road) {
            return true;
        }
        brain
            .map(|b| b.road_tiles.contains(&tile) || b.road_corridor_tiles.contains(&tile))
            .unwrap_or(false)
    };

    // Fast path: a cardinal neighbour that is an *already-carved* Road ⇒
    // connected. A merely-planned spine tile doesn't count — it isn't Road yet,
    // so we still emit a connector that carves the join (otherwise the doormat
    // would sit isolated until the spine carves at runtime).
    if CARDINAL_DIRS.iter().any(|(dx, dy)| {
        chunk_map.tile_kind_at(doormat.0 + dx, doormat.1 + dy) == Some(TileKind::Road)
    }) {
        return DoorConnectorPlan::AlreadyConnected;
    }

    if let Some(path) = cardinal_path_to_target(chunk_map, doormat, radius, &step_blocked, &is_spine)
    {
        return DoorConnectorPlan::Connector(path);
    }

    // No spine within radius — head for home over carvable tiles (continuous).
    let near_home = |tile: (i32, i32)| -> bool {
        tile == home_tile
            || (tile.0 - home_tile.0).abs().max((tile.1 - home_tile.1).abs()) <= 1
    };
    if let Some(path) = cardinal_path_to_target(chunk_map, doormat, radius, &step_blocked, &near_home)
    {
        return DoorConnectorPlan::HomeFallback(path);
    }
    // Boxed in from both spine and home within `radius`: carve at least a single
    // cardinal stub toward home so the doormat is never an isolated / diagonal-
    // only Road tile. The runtime spine carve + desire paths complete the join
    // later. Only truly empty when every cardinal neighbour is unbuildable.
    let stub = CARDINAL_DIRS
        .iter()
        .map(|(dx, dy)| (doormat.0 + dx, doormat.1 + dy))
        .filter(|&t| connector_tile_carvable(chunk_map, t, &step_blocked))
        .min_by_key(|&t| (t.0 - home_tile.0).abs().max((t.1 - home_tile.1).abs()));
    match stub {
        Some(t) => DoorConnectorPlan::HomeFallback(vec![t]),
        None => DoorConnectorPlan::HomeFallback(Vec::new()),
    }
}

/// Is there any `TileKind::Road` tile within `radius` chebyshev of `from`?
/// Used to gate per-door road-carving so we don't pave the entire settlement
/// when many doors all push a fresh Bresenham line to home. The doormat tile
/// itself is always written; only the connection-to-home extension is gated.
#[allow(dead_code)]
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
    structure_index: &StructureIndex,
    tile_changed: &mut EventWriter<crate::world::chunk_streaming::TileChangedEvent>,
    tile: (i32, i32),
) {
    // Anti-corruption guard: never paint Road under a finished structure (mirror
    // of `road_carve_system`'s `StructureIndex` skip) so a doormat carve can't
    // overwrite a building that already sits on the chosen tile.
    if structure_index.0.contains_key(&tile) {
        return;
    }
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
    well_map: Res<WellMap>,
    structure_index: Res<StructureIndex>,
    plot_index: Res<crate::simulation::land::PlotIndex>,
    plant_map: Res<crate::simulation::plants::PlantMap>,
    brains: Res<crate::simulation::organic_settlement::SettlementBrains>,
    settlement_map: Res<crate::simulation::settlement::SettlementMap>,
    mut tile_changed: EventWriter<crate::world::chunk_streaming::TileChangedEvent>,
) {
    if queue.0.is_empty() {
        return;
    }
    // Drain — re-allocate a fresh empty Vec to release the lock on `queue`.
    let drained: Vec<RoadCarveJob> = std::mem::take(&mut queue.0);

    // Helper: write a single tile as Road if it's writable + not blueprinted /
    // bedded / farm-protected. Returns true if the tile was actually flipped.
    // A tile sits inside a well's 5×5 footprint iff its chebyshev distance
    // to any registered well centre is `≤ 2`. The seed-time stamp pre-loads
    // `WellMap` so this gate fires before the in-chain road carve reaches the
    // 9 inner-helix tiles that haven't yet been excavated by
    // `carve_seeded_wells_system`.
    let in_well_footprint = |tile: (i32, i32)| -> bool {
        well_map
            .0
            .keys()
            .any(|c| (c.0 - tile.0).abs() <= 2 && (c.1 - tile.1).abs() <= 2)
    };
    let try_write_road =
        |chunk_map: &mut ChunkMap,
         tile_changed: &mut EventWriter<crate::world::chunk_streaming::TileChangedEvent>,
         tile: (i32, i32)|
         -> bool {
            // Anti-corruption backstop: never paint Road onto a tile that
            // carries a finished structure. With the adaptive widen below this
            // should essentially never fire; it guarantees the carver can't
            // overwrite a building under any plan-vs-carve drift.
            if bp_map.0.contains_key(&tile)
                || bed_map.0.contains_key(&tile)
                || structure_index.0.contains_key(&tile)
                || in_well_footprint(tile)
            {
                return false;
            }
            let surf_z = chunk_map.surface_z_at(tile.0, tile.1);
            let cur = chunk_map.tile_kind_at(tile.0, tile.1);
            let writable = match cur {
                Some(TileKind::Cropland) => false,
                Some(TileKind::Grass) => true,
                Some(TileKind::Scrub) | Some(TileKind::Sand) => true,
                Some(k) if k.is_soil_like() => true,
                _ => false,
            };
            if !writable
                || crate::simulation::land::tile_is_farm_protected(&plot_index, &plant_map, tile)
            {
                return false;
            }
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
            true
        };

    // Per-tile widen sign routes the corridor around standing structures
    // (same `road_widen_tiles` rule the planner's corridor cache uses).
    let widen_blocked = |t: (i32, i32)| structure_index.0.contains_key(&t);
    for job in drained {
        match job {
            RoadCarveJob::Segment {
                from, to, width, ..
            } => {
                let mut x0 = from.0;
                let mut y0 = from.1;
                let x1 = to.0;
                let y1 = to.1;
                let dx_abs = (x1 - x0).abs();
                let dy_abs = (y1 - y0).abs();
                let dx = dx_abs;
                let dy = -dy_abs;
                let sx = if x0 < x1 { 1 } else { -1 };
                let sy = if y0 < y1 { 1 } else { -1 };
                let mut err = dx + dy;

                loop {
                    let is_endpoint = (x0 == from.0 && y0 == from.1) || (x0 == x1 && y0 == y1);
                    if !is_endpoint {
                        let tile = (x0, y0);
                        try_write_road(&mut chunk_map, &mut tile_changed, tile);
                        // Widen per the segment's tier-driven `width` so the
                        // artery fills the gap between facing house frontages.
                        // The widen side flips per tile to step around a
                        // structure on the default side.
                        for widen in crate::simulation::organic_settlement::road_widen_tiles(
                            tile,
                            from,
                            to,
                            width,
                            widen_blocked,
                        ) {
                            try_write_road(&mut chunk_map, &mut tile_changed, widen);
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
            RoadCarveJob::Connector {
                faction_id,
                doormat,
                home,
                width,
            } => {
                // Plan the cardinal path *now*, against the final structure +
                // farm maps, so a later-seeded house can't have invalidated a
                // path computed earlier. The `blocked` predicate mirrors
                // `try_write_road` exactly (bp / bed / structure / well / farm),
                // so every returned tile is guaranteed carvable → the carved
                // road is cardinally continuous with no skipped gaps.
                let brain = settlement_map
                    .first_for_faction(faction_id)
                    .and_then(|sid| brains.0.get(&sid));
                let connector_blocked = |t: (i32, i32)| {
                    bp_map.0.contains_key(&t)
                        || bed_map.0.contains_key(&t)
                        || structure_index.0.contains_key(&t)
                        || in_well_footprint(t)
                        || crate::simulation::land::tile_is_farm_protected(
                            &plot_index,
                            &plant_map,
                            t,
                        )
                };
                let path = match find_door_connector(
                    &chunk_map,
                    brain,
                    home,
                    doormat,
                    12,
                    connector_blocked,
                ) {
                    DoorConnectorPlan::AlreadyConnected => Vec::new(),
                    DoorConnectorPlan::Connector(p) | DoorConnectorPlan::HomeFallback(p) => p,
                };
                // The 1-wide path is the guaranteed-continuous backbone; widen
                // tiles for `width > 1` are best-effort cosmetics that never
                // break it.
                for (i, &tile) in path.iter().enumerate() {
                    try_write_road(&mut chunk_map, &mut tile_changed, tile);
                    if width > 1 {
                        let prev = if i > 0 { path[i - 1] } else { tile };
                        let next = if i + 1 < path.len() { path[i + 1] } else { tile };
                        for widen in crate::simulation::organic_settlement::road_widen_tiles(
                            tile,
                            prev,
                            next,
                            width,
                            widen_blocked,
                        ) {
                            try_write_road(&mut chunk_map, &mut tile_changed, widen);
                        }
                    }
                }
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
    spatial_index: Res<crate::world::spatial::SpatialIndex>,
    stand_reservations: Res<crate::simulation::stand_reservation::StandTileReservations>,
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
                None,
                &chunk_graph,
                &chunk_router,
                &chunk_map,
                &chunk_connectivity,
                &spatial_index,
                &stand_reservations,
                agent_e,
                clock.tick,);
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
            aq.finish_task(&mut ai);
            continue;
        };

        // Peek at the blueprint's deposit list (immutable) so we can snapshot
        // hauler inventories and validate that the bp still exists.
        let bp_info = bp_query
            .get(bp_entity)
            .ok()
            .map(|bp| (bp.deposits, bp.deposit_count));
        let Some((deposits, count)) = bp_info else {
            ai.target_entity = None;
            aq.finish_task(&mut ai);
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
                            Wall {
                                material,
                                owner_faction: Some(bp.faction_id),
                            },
                            crate::simulation::combat::Health::new(material.max_hp()),
                            StructureLabel(material.label()),
                            Transform::from_xyz(world_pos.x, world_pos.y, 0.4),
                            GlobalTransform::default(),
                            Visibility::Visible,
                            InheritedVisibility::default(),
                        ))
                        .id();
                    maps.wall_map.0.insert(tile, wall_entity);
                    maps.wall_constructed.send(WallConstructed {
                        tile,
                        faction: Some(bp.faction_id),
                    });
                    wall_entity
                }
                BuildSiteKind::EdgeWall(material) => {
                    // Thin housing wall: no tile rewrite (the floor stays
                    // passable). One perimeter tile owns 1-2 outward boundary
                    // edges (`edge_sides`); stamp each into `EdgeStructureMap` +
                    // the per-chunk edge cache, one render entity per edge.
                    use crate::simulation::land::TileEdge as TE;
                    let sides = [
                        (edge_side::N, TE::North),
                        (edge_side::E, TE::East),
                        (edge_side::S, TE::South),
                        (edge_side::W, TE::West),
                    ];
                    let mut first_entity: Option<Entity> = None;
                    for (bit, side) in sides {
                        if bp.edge_sides & bit == 0 {
                            continue;
                        }
                        let edge = outward_edge_key(tile, side);
                        let mid = edge_world_mid(edge);
                        let wall_entity = commands
                            .spawn((
                                EdgeWallVisual { material, edge },
                                StructureLabel(material.label()),
                                Transform::from_xyz(mid.x, mid.y, 0.4),
                                GlobalTransform::default(),
                                Visibility::Visible,
                                InheritedVisibility::default(),
                            ))
                            .id();
                        let entry = maps.edge_structures.0.entry(edge).or_default();
                        entry.wall = Some(EdgeWall {
                            material,
                            owner_faction: Some(bp.faction_id),
                            entity: wall_entity,
                        });
                        let state = entry.projected_state();
                        chunk_map.set_edge_state(edge, state);
                        first_entity.get_or_insert(wall_entity);
                    }
                    // Reuse the wall-lifecycle vision-cache invalidation channel.
                    maps.wall_constructed.send(WallConstructed {
                        tile,
                        faction: Some(bp.faction_id),
                    });
                    tile_changed
                        .send(crate::world::chunk_streaming::TileChangedEvent { tx, ty });
                    match first_entity {
                        Some(e) => e,
                        None => {
                            warn!("EdgeWall blueprint at {:?} had empty edge_sides", tile);
                            continue;
                        }
                    }
                }
                BuildSiteKind::EdgeDoor => {
                    // Thin housing door on a tile edge. Passable; opaque when
                    // shut. `tile` is the interior floor tile; the doormat is
                    // the exterior neighbour one step in `door_dir`.
                    let door_edge = bp.door_dir.unwrap_or_else(|| {
                        warn!("EdgeDoor blueprint at {:?} had no door_dir; defaulting East", tile);
                        crate::simulation::land::TileEdge::East
                    });
                    let edge = outward_edge_key(tile, door_edge);
                    let (ddx, ddy) = door_edge.delta();
                    let doormat_tile = (tile.0 + ddx, tile.1 + ddy);
                    let mid = edge_world_mid(edge);
                    let door_entity = commands
                        .spawn((
                            EdgeDoorVisual {
                                edge,
                                dir: door_edge,
                                open: false,
                            },
                            StructureLabel("Door"),
                            Transform::from_xyz(mid.x, mid.y, 0.4),
                            GlobalTransform::default(),
                            Visibility::Visible,
                            InheritedVisibility::default(),
                        ))
                        .id();
                    let entry = maps.edge_structures.0.entry(edge).or_default();
                    entry.door = Some(EdgeDoorRef {
                        entity: door_entity,
                        open: false,
                        faction_id: bp.faction_id,
                        dir: door_edge,
                    });
                    let state = entry.projected_state();
                    chunk_map.set_edge_state(edge, state);
                    doormat_reservations.0.insert(
                        doormat_tile,
                        crate::simulation::doormat::DoormatEntry {
                            owner_door: door_entity,
                            door_tile: tile,
                            dir: door_edge,
                        },
                    );
                    write_road_tile(
                        &mut *chunk_map,
                        &maps.structure_index,
                        &mut tile_changed,
                        doormat_tile,
                    );
                    if let Some(home_tile) =
                        registry.factions.get(&bp.faction_id).map(|f| f.home_tile)
                    {
                        queue_door_connector(
                            &mut road_carve_queue,
                            bp.faction_id,
                            doormat_tile,
                            home_tile,
                        );
                    }
                    door_entity
                }
                BuildSiteKind::Bed => {
                    let bed = Bed {
                        owner: None,
                        tier: best_bed_for(&build_techs),
                        owning_faction: Some(bp.faction_id),
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
                    let bed = Bed {
                        owner: None,
                        tier: BedTier::Crude,
                        owning_faction: Some(bp.faction_id),
                    };
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
                // Poor-housing sleeping mat: a Bed at the SleepingMat tier so
                // HomeBed assignment + sleep dispatch treat it like any bed,
                // but it only grants 1.25× recovery. The label carries the
                // material name (e.g. "Reed Mat") for hover/activity log.
                BuildSiteKind::SleepingMat(mat) => {
                    let bed = Bed {
                        owner: None,
                        tier: BedTier::SleepingMat,
                        owning_faction: Some(bp.faction_id),
                    };
                    let _ = mat;
                    let bed_entity = commands
                        .spawn((
                            bed,
                            StructureLabel("Sleeping Mat"),
                            Transform::from_xyz(world_pos.x, world_pos.y, 0.32),
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
                // Poor-housing lightweight shelter: a LeanTo-tier TentShelter
                // registered in ShelterMap (relief) + StructureIndex (via the
                // StructureLabel hook, for MP replication). Not packable.
                BuildSiteKind::LightShelter(_mat) => {
                    let e = commands
                        .spawn((
                            TentShelter {
                                tier: ShelterTier::LeanTo,
                            },
                            StructureLabel("Lean-To"),
                            Transform::from_xyz(world_pos.x, world_pos.y, 0.4),
                            GlobalTransform::default(),
                            Visibility::Visible,
                            InheritedVisibility::default(),
                        ))
                        .id();
                    e
                }
                BuildSiteKind::Campfire => {
                    // Manual / chief / autonomous Campfire finalize. Role
                    // comes from the blueprint (organic civic pressure +
                    // Longhouse interior set it explicitly); fall back to
                    // Civic for legacy / manual right-click sites.
                    let role = bp.hearth_role.unwrap_or(HearthRole::Civic);
                    let campfire = Campfire {
                        tier: best_hearth_for(&build_techs),
                        role,
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
                    maps.campfire_map
                        .0
                        .insert(tile, CampfireEntry { entity: campfire_entity, role });
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
                            faction_id: bp.faction_id,
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
                    // skips both endpoints. Connector chosen by
                    // `find_door_connector`: a bounded cardinal search that
                    // returns the carvable path joining the doormat to the
                    // nearest carved/planned spine tile, routing around
                    // structures and farms — no diagonal-only joins, no
                    // radial wagon-wheel into the base.
                    write_road_tile(
                        &mut *chunk_map,
                        &maps.structure_index,
                        &mut tile_changed,
                        doormat_tile,
                    );
                    if let Some(home_tile) =
                        registry.factions.get(&bp.faction_id).map(|f| f.home_tile)
                    {
                        queue_door_connector(
                            &mut road_carve_queue,
                            bp.faction_id,
                            doormat_tile,
                            home_tile,
                        );
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
                    // Wells are normally converted to a staged `WellSite` by
                    // `well::convert_well_blueprint_system` before they reach
                    // finalize; this arm is a degenerate fallback (flat well,
                    // shaft == surface).
                    let well_entity = commands
                        .spawn((
                            Well {
                                faction_id: bp.faction_id,
                                shaft_tile: tile,
                                bottom_z: bp.target_z,
                                // `surf_z` is the ground surface, not the
                                // shaft bottom — `well_spec_of` derives the
                                // helix length from `surf_z - bottom_z`.
                                surf_z: {
                                    let s = chunk_map.surface_z_at(tile.0, tile.1);
                                    if s >= Z_MIN {
                                        s.clamp(Z_MIN, Z_MAX) as i8
                                    } else {
                                        bp.target_z
                                    }
                                },
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
                BuildSiteKind::Pen => commands
                    .spawn((
                        crate::simulation::husbandry::Pen {
                            faction_id: bp.faction_id,
                            tile,
                            capacity: 4,
                            // Cattle / Pig / Dog default — Stables handle horses.
                            species_mask: crate::simulation::husbandry::SPECIES_CATTLE
                                | crate::simulation::husbandry::SPECIES_PIG
                                | crate::simulation::husbandry::SPECIES_DOG,
                        },
                        StructureLabel(BuildSiteKind::Pen.label()),
                        Transform::from_xyz(world_pos.x, world_pos.y, 0.2),
                        GlobalTransform::default(),
                        Visibility::Visible,
                        InheritedVisibility::default(),
                    ))
                    .id(),
                BuildSiteKind::Stable => commands
                    .spawn((
                        crate::simulation::husbandry::Stable {
                            faction_id: bp.faction_id,
                            tile,
                            capacity: 2,
                            species_mask: crate::simulation::husbandry::SPECIES_HORSE
                                | crate::simulation::husbandry::SPECIES_CATTLE,
                        },
                        StructureLabel(BuildSiteKind::Stable.label()),
                        Transform::from_xyz(world_pos.x, world_pos.y, 0.3),
                        GlobalTransform::default(),
                        Visibility::Visible,
                        InheritedVisibility::default(),
                    ))
                    .id(),
                BuildSiteKind::FeedTrough => commands
                    .spawn((
                        crate::simulation::husbandry::FeedTrough {
                            faction_id: bp.faction_id,
                            tile,
                            stock_g: 0,
                            capacity_g: 20_000,
                        },
                        StructureLabel(BuildSiteKind::FeedTrough.label()),
                        Transform::from_xyz(world_pos.x, world_pos.y, 0.2),
                        GlobalTransform::default(),
                        Visibility::Visible,
                        InheritedVisibility::default(),
                    ))
                    .id(),
                BuildSiteKind::HitchingPost => commands
                    .spawn((
                        crate::simulation::husbandry::HitchingPost::new(bp.faction_id, tile),
                        StructureLabel(BuildSiteKind::HitchingPost.label()),
                        Transform::from_xyz(world_pos.x, world_pos.y, 0.2),
                        GlobalTransform::default(),
                        Visibility::Visible,
                        InheritedVisibility::default(),
                    ))
                    .id(),
                BuildSiteKind::VehicleYard => commands
                    .spawn((
                        crate::simulation::vehicle::VehicleYard {
                            faction_id: bp.faction_id,
                            tile,
                        },
                        StructureLabel(BuildSiteKind::VehicleYard.label()),
                        Transform::from_xyz(world_pos.x, world_pos.y, 0.2),
                        GlobalTransform::default(),
                        Visibility::Visible,
                        InheritedVisibility::default(),
                    ))
                    .id(),
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
                road_carve_queue.0.push(RoadCarveJob::Segment {
                    faction_id: bp.faction_id,
                    from: tile,
                    to: faction.home_tile,
                    width: DEFAULT_ROAD_WIDTH,
                });
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
            ai.target_entity = None;
            aq.finish_task(&mut ai);
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

/// Chebyshev radius for the home-tile / settlement-market-tile backstop used
/// when a legacy untagged bed has no `PlotIndex` entry. Same magnitude as the
/// pre-fix box but chebyshev (matches the rest of the codebase) and only the
/// legacy safety net, not the primary filter.
pub(crate) const BED_FALLBACK_RADIUS: i32 = 30;

/// Pure predicate: is a bed with `bed_owning_faction` at `bed_tile` claimable
/// by a worker rooted at `viewer_root`?
///
/// Pure-fn shape so tests can drive it without an `App`. The system caller
/// wraps `PlotIndex` + `Query<&Plot>` into the `plot_faction_at` closure and
/// `FactionRegistry::root_faction` into `root_of`.
///
/// Resolution order:
/// 1. `bed_owning_faction == Some(fid)` → eligible iff `root_of(fid) == viewer_root`.
/// 2. Untagged (legacy / pre-tag spawn): `plot_faction_at(bed_tile)` returns the
///    plot's `Plot.faction_id` (if any). Compare `root_of(plot_faction)` to
///    `viewer_root` — that decision is final when the bed sits in a plot.
/// 3. Untagged and not in any plot → accept if the bed tile sits within
///    chebyshev `BED_FALLBACK_RADIUS` of any tile in `anchor_tiles` (the
///    viewer-faction's home + every owned `Settlement.market_tile`).
///
/// Households (`HouseholdMember`) consult the parent village's root via
/// `root_of`, so a household member can claim a bed inside the parent
/// village's residential plot.
pub(crate) fn bed_eligible_for_faction(
    bed_owning_faction: Option<u32>,
    bed_tile: (i32, i32),
    viewer_root: u32,
    root_of: &impl Fn(u32) -> u32,
    plot_faction_at: &impl Fn((i32, i32)) -> Option<u32>,
    anchor_tiles: &[(i32, i32)],
) -> bool {
    // 1. Tagged beds: rooted-faction equality.
    if let Some(bed_faction) = bed_owning_faction {
        return root_of(bed_faction) == viewer_root;
    }
    // 2. Untagged: plot-derived ownership is authoritative when present.
    if let Some(plot_faction) = plot_faction_at(bed_tile) {
        return root_of(plot_faction) == viewer_root;
    }
    // 3. Untagged + no plot: settlement-anchor chebyshev backstop.
    anchor_tiles.iter().any(|&(ax, ay)| {
        let dx = (bed_tile.0 - ax).abs();
        let dy = (bed_tile.1 - ay).abs();
        dx.max(dy) <= BED_FALLBACK_RADIUS
    })
}

/// Pure predicate: should `assign_beds_system` clear this bed's `owner` (so it
/// re-enters the claim pool)? Keeps `HomeBed == bed <=> Bed.owner == person` an
/// invariant. `owner_alive` = the owner entity still exists; `owner_home_bed` =
/// the owner's current `HomeBed` target; `owner_bed_eligible` = the bed passes
/// `bed_eligible_for_faction` for the owner's root.
pub(crate) fn bed_owner_is_stale(
    bed_entity: Entity,
    owner_alive: bool,
    owner_home_bed: Option<Entity>,
    owner_bed_eligible: bool,
) -> bool {
    !owner_alive                              // death / despawn left the claim dangling
        || owner_home_bed != Some(bed_entity) // spouse-relocation left old bed tagged
        || !owner_bed_eligible                // plot changed hands / no longer eligible
}

/// Pure predicate: is `person`'s `HomeBed` claim stale, so the homeless pass
/// should reassign them? `home_bed` = the claimed entity (None = no claim);
/// `bed_alive` = the claimed bed entity still exists; `bed_owner` = its `owner`;
/// `bed_eligible` = it passes eligibility for the person's root.
pub(crate) fn home_bed_claim_is_stale(
    person: Entity,
    home_bed: Option<Entity>,
    bed_alive: bool,
    bed_owner: Option<Entity>,
    bed_eligible: bool,
) -> bool {
    match home_bed {
        None => true,                              // no claim
        Some(_) if !bed_alive => true,             // claimed bed despawned
        Some(_) => bed_owner != Some(person) || !bed_eligible, // phantom / ineligible
    }
}

/// Pure predicate: should `assign_beds_system` cancel an in-flight
/// `Task::Sleep { bed: None }` so the sleeper re-routes onto a now-valid bed?
/// `valid_claim` = a live bed reciprocally owned by the sleeper; `on_bed` = the
/// sleeper already stands on the bed tile; `recent_sleep_route_failure` = a
/// SLEEP routing failure inside the `MethodHistory` TTL (cooldown that prevents
/// per-pass churn against a genuinely unreachable bed).
pub(crate) fn should_reroute_bedless_sleeper(
    valid_claim: bool,
    on_bed: bool,
    recent_sleep_route_failure: bool,
) -> bool {
    valid_claim && !on_bed && !recent_sleep_route_failure
}
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
    mut person_query: Query<
        (
            Entity,
            &FactionMember,
            &Transform,
            Option<&HomeBed>,
            Option<&crate::simulation::memory::RelationshipMemory>,
            Option<&crate::simulation::reproduction::BiologicalSex>,
            Option<&crate::simulation::reproduction::HouseholdMember>,
            Option<&mut PersonAI>,
            Option<&mut crate::simulation::typed_task::ActionQueue>,
        ),
        With<Person>,
    >,
    bed_map: Res<BedMap>,
    mut bp_map: ResMut<BlueprintMap>,
    bp_query: Query<&Blueprint>,
    faction_registry: Res<FactionRegistry>,
    clock: Res<SimClock>,
    chunk_map: Res<ChunkMap>,
    plot_index: Res<crate::simulation::land::PlotIndex>,
    plot_q: Query<&crate::simulation::land::Plot>,
    settlement_map: Res<crate::simulation::settlement::SettlementMap>,
    settlement_q: Query<&crate::simulation::settlement::Settlement>,
    method_history_q: Query<&crate::simulation::htn::MethodHistory>,
) {
    use crate::simulation::reproduction::BiologicalSex;
    use crate::simulation::settlement_bootstrap::SPOUSE_AFFINITY;

    if clock.tick % 30 != 0 {
        return;
    }

    let mut claimed_this_pass: AHashSet<Entity> = AHashSet::new();

    // Reverse lookup: bed entity → tile position. BedMap is sparse so the
    // collect is cheap.
    let bed_pos_by_entity: AHashMap<Entity, (i32, i32)> =
        bed_map.0.iter().map(|(&pos, &e)| (e, pos)).collect();

    // ── Resolvers + bed-claim reconciliation (run before any assignment) ─────
    // `root_of` / `plot_faction_at` / `anchors_by_root` back the eligibility
    // predicate used by every pass below and by the stale-owner sweep. Hoisted
    // here so a bed freed from a dead/relocated owner re-enters the `available`
    // pool this same tick.
    let root_of = |fid: u32| faction_registry.root_faction(fid);
    let plot_faction_at = |tile: (i32, i32)| -> Option<u32> {
        let plot_id = plot_index.plot_at(tile.0, tile.1)?;
        let plot_entity = *plot_index.by_id.get(&plot_id)?;
        let plot = plot_q.get(plot_entity).ok()?;
        Some(plot.faction_id)
    };
    let mut anchors_by_root: AHashMap<u32, Vec<(i32, i32)>> = AHashMap::new();
    for (faction_id, data) in faction_registry.factions.iter() {
        let root = faction_registry.root_faction(*faction_id);
        anchors_by_root.entry(root).or_default().push(data.home_tile);
    }
    for (faction_id, ids) in settlement_map.by_faction.iter() {
        let root = faction_registry.root_faction(*faction_id);
        let bucket = anchors_by_root.entry(root).or_default();
        for sid in ids {
            if let Some(&se) = settlement_map.by_id.get(sid) {
                if let Ok(s) = settlement_q.get(se) {
                    bucket.push(s.market_tile);
                }
            }
        }
    }
    let empty_anchors: Vec<(i32, i32)> = Vec::new();

    // Clear leaked `Bed.owner` so `HomeBed == bed <=> Bed.owner == person`
    // stays an invariant: owner entity gone (death / despawn), owner's `HomeBed`
    // no longer points back (spouse-relocation left the old bed tagged), or the
    // bed is no longer eligible for the owner's root faction. A cleared bed
    // re-enters the claim pool below.
    {
        let mut to_clear: Vec<Entity> = Vec::new();
        for (&pos, &bed_entity) in bed_map.0.iter() {
            let Ok(bed) = bed_query.get(bed_entity) else {
                continue;
            };
            let Some(owner) = bed.owner else { continue };
            let owning_faction = bed.owning_faction;
            let drop = match person_query.get(owner) {
                Ok((_, member, _, home_bed, _, _, _, _, _)) => {
                    let owner_root = faction_registry.root_faction(member.faction_id);
                    let anchors = anchors_by_root.get(&owner_root).unwrap_or(&empty_anchors);
                    let eligible = bed_eligible_for_faction(
                        owning_faction,
                        pos,
                        owner_root,
                        &root_of,
                        &plot_faction_at,
                        anchors,
                    );
                    bed_owner_is_stale(bed_entity, true, home_bed.and_then(|h| h.0), eligible)
                }
                Err(_) => bed_owner_is_stale(bed_entity, false, None, false),
            };
            if drop {
                to_clear.push(bed_entity);
            }
        }
        for bed_entity in to_clear {
            if let Ok(mut bed) = bed_query.get_mut(bed_entity) {
                bed.owner = None;
            }
        }
    }

    // Snapshot every person's HomeBed entity, sex, faction, household, and
    // current tile so Pass 0 (seeded-spouse pairing) and Pass A can resolve
    // partner data without re-querying.
    struct PartnerInfo {
        sex: Option<BiologicalSex>,
        home_bed: Option<Entity>,
        faction_id: u32,
        household_id: Option<u32>,
        tile: (i32, i32),
    }
    let partner_info: AHashMap<Entity, PartnerInfo> = person_query
        .iter()
        .map(|(e, fm, tr, hb, _, sex, hh, _, _)| {
            let tx = (tr.translation.x / crate::world::terrain::TILE_SIZE).floor() as i32;
            let ty = (tr.translation.y / crate::world::terrain::TILE_SIZE).floor() as i32;
            (
                e,
                PartnerInfo {
                    sex: sex.copied(),
                    home_bed: hb.and_then(|h| h.0),
                    faction_id: fm.faction_id,
                    household_id: hh.map(|h| h.household_id),
                    tile: (tx, ty),
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

    // ── Pass 0: pair seeded spouses to adjacent unclaimed beds atomically.
    //
    // For every unhoused `HouseholdMember`, walk their `RelationshipMemory`
    // for the highest-affinity same-household opposite-sex peer at
    // `SPOUSE_AFFINITY (79)`. If both are unhoused, pick the closest
    // unclaimed bed to either spouse, then the closest still-unclaimed bed
    // within `PARTNER_PROXIMITY_RADIUS` Manhattan as the partner's bed.
    // Assign both atomically. This eliminates the "female processed first,
    // partner has no bed yet" race in the homeless faction pass — by the
    // time Pass A and the homeless faction pass run, seeded spouses are
    // already paired into adjacent beds inside their dwelling footprint.
    let mut paired_this_pass: AHashSet<Entity> = AHashSet::new();
    for (person, member, _transform, home_bed_opt, rel_opt, _sex_opt, household_opt, _, _) in
        &person_query
    {
        if member.faction_id == SOLO {
            continue;
        }
        if home_bed_opt.and_then(|h| h.0).is_some() {
            continue;
        }
        if paired_this_pass.contains(&person) {
            continue;
        }
        let Some(my_hh) = household_opt else { continue };
        let Some(rel) = rel_opt else { continue };
        let Some(my_info) = partner_info.get(&person) else {
            continue;
        };
        let my_sex = match my_info.sex {
            Some(s) => s,
            None => continue,
        };

        // Find best same-household opposite-sex spouse anchor at
        // SPOUSE_AFFINITY (or higher) that is also currently unhoused.
        let mut anchor: Option<Entity> = None;
        let mut anchor_aff: i8 = i8::MIN;
        for slot in &rel.entries {
            let Some(entry) = slot else { continue };
            if entry.entity == person {
                continue;
            }
            if entry.affinity < SPOUSE_AFFINITY {
                continue;
            }
            if paired_this_pass.contains(&entry.entity) {
                continue;
            }
            let Some(p_info) = partner_info.get(&entry.entity) else {
                continue;
            };
            if p_info.faction_id != member.faction_id {
                continue;
            }
            if p_info.household_id != Some(my_hh.household_id) {
                continue;
            }
            let Some(p_sex) = p_info.sex else { continue };
            if p_sex == my_sex {
                continue;
            }
            if p_info.home_bed.is_some() {
                continue;
            }
            if entry.affinity > anchor_aff {
                anchor_aff = entry.affinity;
                anchor = Some(entry.entity);
            }
        }
        let Some(partner) = anchor else { continue };
        let partner_info_ref = match partner_info.get(&partner) {
            Some(p) => p,
            None => continue,
        };

        // Pick the unclaimed bed nearest to either spouse's current tile.
        let my_pos = my_info.tile;
        let partner_pos = partner_info_ref.tile;
        let mut first_bed: Option<(Entity, (i32, i32))> = None;
        let mut first_score = i32::MAX;
        for (&pos, &bed_e) in &bed_map.0 {
            if claimed_this_pass.contains(&bed_e) {
                continue;
            }
            match bed_query.get(bed_e) {
                Ok(b) if b.owner.is_none() => {}
                _ => continue,
            }
            let d_me = (pos.0 - my_pos.0).abs() + (pos.1 - my_pos.1).abs();
            let d_p = (pos.0 - partner_pos.0).abs() + (pos.1 - partner_pos.1).abs();
            let d = d_me.min(d_p);
            if d < first_score {
                first_score = d;
                first_bed = Some((bed_e, pos));
            }
        }
        let Some((first_e, first_pos)) = first_bed else {
            continue;
        };
        // Pick the closest *still-unclaimed* bed within
        // PARTNER_PROXIMITY_RADIUS Manhattan of the first as partner's bed.
        let mut second_bed: Option<Entity> = None;
        let mut second_score = i32::MAX;
        for (&pos, &bed_e) in &bed_map.0 {
            if bed_e == first_e {
                continue;
            }
            if claimed_this_pass.contains(&bed_e) {
                continue;
            }
            match bed_query.get(bed_e) {
                Ok(b) if b.owner.is_none() => {}
                _ => continue,
            }
            let d = (pos.0 - first_pos.0).abs() + (pos.1 - first_pos.1).abs();
            if d > PARTNER_PROXIMITY_RADIUS {
                continue;
            }
            if d < second_score {
                second_score = d;
                second_bed = Some(bed_e);
            }
        }
        let Some(second_e) = second_bed else {
            continue;
        };

        // Assign atomically.
        if let Ok(mut b) = bed_query.get_mut(first_e) {
            b.owner = Some(person);
        }
        if let Ok(mut b) = bed_query.get_mut(second_e) {
            b.owner = Some(partner);
        }
        commands.entity(person).insert(HomeBed(Some(first_e)));
        commands.entity(partner).insert(HomeBed(Some(second_e)));
        claimed_this_pass.insert(first_e);
        claimed_this_pass.insert(second_e);
        paired_this_pass.insert(person);
        paired_this_pass.insert(partner);
    }

    // ── Pass A: re-evaluate already-housed women whose partner lives far away.
    for (person, member, _transform, home_bed_opt, rel_opt, sex_opt, household_opt, _, _) in
        &person_query
    {
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

    // ── Pass A.5: narrow household-spouse relocation for seeded couples.
    //
    // Pass A's `REASSIGN_AFFINITY_THRESHOLD = 80` deliberately excludes
    // seeded spouses at 79 because its fallback queues a Bed blueprint
    // outside any Hut/Longhouse footprint. This narrower branch only
    // *moves* an already-housed household-member to an unclaimed bed within
    // `PARTNER_PROXIMITY_RADIUS` of their same-household opposite-sex
    // spouse's bed — never queues a blueprint. Closes the loop for the rare
    // case where Pass 0 couldn't pair them in a single pass (mid-game
    // household formation, mid-game spawn, abstract-faction materialisation).
    for (person, _member, _transform, home_bed_opt, rel_opt, _sex_opt, household_opt, _, _) in
        &person_query
    {
        let Some(my_hh) = household_opt else { continue };
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
        let Some(my_info) = partner_info.get(&person) else {
            continue;
        };
        let my_sex = match my_info.sex {
            Some(s) => s,
            None => continue,
        };

        // Find the spouse anchor: same household, opposite-sex, affinity
        // ≥ SPOUSE_AFFINITY, with a HomeBed of their own.
        let mut partner_bed_pos: Option<(i32, i32)> = None;
        let mut best_aff: i8 = i8::MIN;
        for slot in &rel.entries {
            let Some(entry) = slot else { continue };
            if entry.entity == person {
                continue;
            }
            if entry.affinity < SPOUSE_AFFINITY {
                continue;
            }
            let Some(p_info) = partner_info.get(&entry.entity) else {
                continue;
            };
            if p_info.household_id != Some(my_hh.household_id) {
                continue;
            }
            let Some(p_sex) = p_info.sex else { continue };
            if p_sex == my_sex {
                continue;
            }
            let Some(p_bed_e) = p_info.home_bed else {
                continue;
            };
            let Some(&p_bed_pos) = bed_pos_by_entity.get(&p_bed_e) else {
                continue;
            };
            if entry.affinity > best_aff {
                best_aff = entry.affinity;
                partner_bed_pos = Some(p_bed_pos);
            }
        }
        let Some(partner_pos) = partner_bed_pos else {
            continue;
        };
        let cur_dist =
            (my_bed_pos.0 - partner_pos.0).abs() + (my_bed_pos.1 - partner_pos.1).abs();
        if cur_dist <= PARTNER_PROXIMITY_RADIUS {
            continue;
        }
        // Look for a free unclaimed bed within PARTNER_PROXIMITY_RADIUS of
        // the partner. No blueprint fallback — if none, do nothing.
        let mut candidate: Option<Entity> = None;
        for (&pos, &bed_e) in &bed_map.0 {
            if bed_e == my_bed || claimed_this_pass.contains(&bed_e) {
                continue;
            }
            let d = (pos.0 - partner_pos.0).abs() + (pos.1 - partner_pos.1).abs();
            if d > PARTNER_PROXIMITY_RADIUS {
                continue;
            }
            match bed_query.get(bed_e) {
                Ok(b) if b.owner.is_none() => {
                    candidate = Some(bed_e);
                    break;
                }
                _ => {}
            }
        }
        if let Some(new_bed) = candidate {
            if let Ok(mut old) = bed_query.get_mut(my_bed) {
                old.owner = None;
            }
            if let Ok(mut new) = bed_query.get_mut(new_bed) {
                new.owner = Some(person);
            }
            commands.entity(person).insert(HomeBed(Some(new_bed)));
            claimed_this_pass.insert(new_bed);
        }
    }

    // ── Faction pass ─────────────────────────────────────────────────────────
    struct Homeless {
        person: Entity,
        pos: (i32, i32),
        partner_bed: Option<(i32, i32)>,
    }
    // Bucket by root_faction so households share the village's bed pool.
    let mut homeless_by_root: AHashMap<u32, Vec<Homeless>> = AHashMap::new();
    for (person, member, transform, home_bed, rel_opt, sex_opt, _household_opt, _, _) in
        &person_query
    {
        if member.faction_id == SOLO {
            continue;
        }
        let root = faction_registry.root_faction(member.faction_id);
        // Homeless = no claim, despawned bed, mismatched/non-reciprocal claim,
        // or a claim that is no longer faction-eligible.
        let claim = home_bed.and_then(|h| h.0);
        let stale = match claim {
            Some(bed_entity) => match bed_query.get(bed_entity) {
                Ok(bed) => {
                    let bed_tile = bed_pos_by_entity
                        .get(&bed_entity)
                        .copied()
                        .unwrap_or((i32::MIN, i32::MIN));
                    let eligible = bed_eligible_for_faction(
                        bed.owning_faction,
                        bed_tile,
                        root,
                        &root_of,
                        &plot_faction_at,
                        anchors_by_root.get(&root).unwrap_or(&empty_anchors),
                    );
                    home_bed_claim_is_stale(person, claim, true, bed.owner, eligible)
                }
                Err(_) => home_bed_claim_is_stale(person, claim, false, None, false),
            },
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
        homeless_by_root
            .entry(root)
            .or_default()
            .push(Homeless {
                person,
                pos: (x, y),
                partner_bed,
            });
    }

    for (root, homeless) in homeless_by_root {
        let anchors = anchors_by_root.get(&root).unwrap_or(&empty_anchors);
        let mut available: Vec<(Entity, (i32, i32))> = bed_map
            .0
            .iter()
            .filter_map(|(pos, &bed_entity)| {
                if claimed_this_pass.contains(&bed_entity) {
                    return None;
                }
                let bed = bed_query.get(bed_entity).ok()?;
                if bed.owner.is_some() {
                    return None;
                }
                if !bed_eligible_for_faction(
                    bed.owning_faction,
                    *pos,
                    root,
                    &root_of,
                    &plot_faction_at,
                    anchors,
                ) {
                    return None;
                }
                Some((bed_entity, *pos))
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
    let solo_anchors = anchors_by_root.get(&SOLO).unwrap_or(&empty_anchors);
    for (person, member, transform, home_bed, _, _, _, _, _) in &person_query {
        if member.faction_id != SOLO {
            continue;
        }
        let claim = home_bed.and_then(|h| h.0);
        let stale = match claim {
            Some(bed_entity) => match bed_query.get(bed_entity) {
                Ok(bed) => {
                    let bed_tile = bed_pos_by_entity
                        .get(&bed_entity)
                        .copied()
                        .unwrap_or((i32::MIN, i32::MIN));
                    let eligible = bed_eligible_for_faction(
                        bed.owning_faction,
                        bed_tile,
                        SOLO,
                        &root_of,
                        &plot_faction_at,
                        solo_anchors,
                    );
                    home_bed_claim_is_stale(person, claim, true, bed.owner, eligible)
                }
                Err(_) => home_bed_claim_is_stale(person, claim, false, None, false),
            },
            None => true,
        };
        if !stale {
            continue;
        }
        let tx = (transform.translation.x / crate::world::terrain::TILE_SIZE).floor() as i32;
        let ty = (transform.translation.y / crate::world::terrain::TILE_SIZE).floor() as i32;

        // Claim the nearest unclaimed eligible bed (eligibility per the helper:
        // SOLO-tagged or untagged-with-no-other-faction's-plot — `solo_anchors`
        // backstops the legacy radius case).
        let mut best_bed: Option<(Entity, i32)> = None;
        for (&bpos, &bed_entity) in &bed_map.0 {
            if claimed_this_pass.contains(&bed_entity) {
                continue;
            }
            let Ok(bed) = bed_query.get(bed_entity) else {
                continue;
            };
            if bed.owner.is_some() {
                continue;
            }
            if !bed_eligible_for_faction(
                bed.owning_faction,
                bpos,
                SOLO,
                &root_of,
                &plot_faction_at,
                solo_anchors,
            ) {
                continue;
            }
            let d = (bpos.0 - tx).abs() + (bpos.1 - ty).abs();
            if best_bed.map(|(_, bd)| d < bd).unwrap_or(true) {
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

    // ── Re-route any valid-bed worker still sleeping bedless ────────────────
    // Covers both workers freshly assigned a `HomeBed` this pass AND those who
    // fell back to `Task::Sleep { bed: None }` after a transient dusk routing
    // failure. A worker with a live, reciprocal `HomeBed` who isn't already on
    // the bed and has no recent SLEEP routing failure gets its chain cancelled
    // so the next `htn_sleep_dispatch_system` tick routes to
    // `Sleep { bed: Some(_) }`. The `recently_failed_count` guard caps retries
    // at one per `MethodHistory` TTL window — a genuinely-unreachable bed never
    // causes per-pass churn (the worker still recovers in place at 1× meanwhile).
    let now = clock.tick;
    for (person, _member, transform, home_bed, _, _, _, ai_opt, aq_opt) in person_query.iter_mut() {
        let (Some(mut ai), Some(mut aq)) = (ai_opt, aq_opt) else {
            continue;
        };
        if aq.current.as_sleep() != Some(None) {
            continue;
        }
        let Some(bed_entity) = home_bed.and_then(|h| h.0) else {
            continue;
        };
        // Only a live, reciprocal claim earns a reroute.
        let valid_claim = matches!(bed_query.get(bed_entity), Ok(bed) if bed.owner == Some(person));
        let on_bed = bed_pos_by_entity.get(&bed_entity).is_some_and(|&bed_tile| {
            let cur = (
                (transform.translation.x / crate::world::terrain::TILE_SIZE).floor() as i32,
                (transform.translation.y / crate::world::terrain::TILE_SIZE).floor() as i32,
            );
            cur == bed_tile
        });
        let recent_sleep_route_failure = method_history_q
            .get(person)
            .map(|h| h.recently_failed_count(crate::simulation::htn::MethodId::SLEEP, now) > 0)
            .unwrap_or(false);
        if should_reroute_bedless_sleeper(valid_claim, on_bed, recent_sleep_route_failure) {
            aq.cancel_chain(&mut ai);
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
    spatial_index: Res<crate::world::spatial::SpatialIndex>,
    stand_reservations: Res<crate::simulation::stand_reservation::StandTileReservations>,
    clock: Res<crate::simulation::SimClock>,
) {
    let now = clock.tick;
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
        } else if let Some(entry) = maps.campfire_map.0.remove(&tile) {
            removed = Some((entry.entity, BuildSiteKind::Campfire, false));
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
        let water_anchored_refund_tile: Option<(i32, i32)> =
            if matches!(kind, BuildSiteKind::Bridge | BuildSiteKind::Dam) {
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
                            owner_household: None,
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
                    None,
                    &chunk_graph,
                    &chunk_router,
                    &chunk_map,
                    &chunk_connectivity,
                    &spatial_index,
                    &stand_reservations,
                    agent_entity,
                    now,);
                if !dispatched {
                    aq.cancel_chain(&mut ai);
                }
            } else {
                // `aq.advance()` above already promoted Task::Idle into
                // current — re-assert FSM state to match.
                aq.assert_idle(&mut ai);
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
    // Role to stamp on a Campfire. Ignored for non-Campfire kinds.
    // Seed-time `BuildIntent::Single(BuildSiteKind::Campfire)` always
    // arrives here from organic civic pressure → `Civic`.
    hearth_role: HearthRole,
) {
    used.insert(tile);
    let world_pos = tile_to_world(tile.0, tile.1);

    match kind {
        BuildSiteKind::Bed => {
            let bed = Bed {
                owner: None,
                tier: best_bed_for(seed_techs),
                owning_faction: Some(faction_id),
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
            let bed = Bed {
                owner: None,
                tier: BedTier::Crude,
                owning_faction: Some(faction_id),
            };
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
            // ShelterMap registration handled by the TentShelter on_add hook.
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
            // ShelterMap registration handled by the TentShelter on_add hook.
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
        // Poor-housing primitives at seed time (emergency shelter pressure in
        // an Established/Developed start that can't procure wall material).
        BuildSiteKind::SleepingMat(_) => {
            let bed = Bed {
                owner: None,
                tier: BedTier::SleepingMat,
                owning_faction: Some(faction_id),
            };
            let e = commands
                .spawn((
                    bed,
                    StructureLabel("Sleeping Mat"),
                    Transform::from_xyz(world_pos.x, world_pos.y, 0.32),
                    GlobalTransform::default(),
                    Visibility::Visible,
                    InheritedVisibility::default(),
                    crate::world::spatial::Indexed::new(crate::world::spatial::IndexedKind::Bed),
                ))
                .id();
            maps.bed_map.0.insert(tile, e);
        }
        BuildSiteKind::LightShelter(_) => {
            // ShelterMap registration handled by the TentShelter on_add hook.
            commands.spawn((
                TentShelter {
                    tier: ShelterTier::LeanTo,
                },
                StructureLabel("Lean-To"),
                Transform::from_xyz(world_pos.x, world_pos.y, 0.4),
                GlobalTransform::default(),
                Visibility::Visible,
                InheritedVisibility::default(),
            ));
        }
        BuildSiteKind::Campfire => {
            let campfire = Campfire {
                tier: best_hearth_for(seed_techs),
                role: hearth_role,
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
            maps.campfire_map.0.insert(
                tile,
                CampfireEntry {
                    entity: e,
                    role: hearth_role,
                },
            );
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
        // Wells take a dedicated path (`well::stamp_seeded_well` via
        // `seed_apply_intent`) because they need an aquifer-resolved
        // `WellSpec` plus a footprint-aware placement search. Reaching this
        // branch means a caller bypassed that route — drop without stamping.
        BuildSiteKind::Well => {
            debug_assert!(
                false,
                "BuildSiteKind::Well must route through well::stamp_seeded_well"
            );
            return;
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
    mut seed_reservation: ResMut<crate::simulation::seed_reservation::SeedReservation>,
    globe: Res<Globe>,
    brains: Res<crate::simulation::organic_settlement::SettlementBrains>,
    settlement_map: Res<crate::simulation::settlement::SettlementMap>,
    structure_index: Res<StructureIndex>,
    archetypes: Res<crate::simulation::organic_settlement::BuildingArchetypeCatalog>,
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

    // The seed pipeline drives the organic intent path directly (one
    // pipeline, two consumers): `append_pressures_for_faction` →
    // `pressure_to_intent` → `seed_apply_intent`. No blueprints, no
    // workers, no `generate_candidates`. The civic-milestone gate is
    // routed through `CivicGate::Seed(options.maturity)` so a Founder
    // start re-imposes runtime gates while Established/Developed seeds
    // civic capital regardless of pop. The placeholder `BlueprintMap` /
    // pending-counts table satisfy the helpers' signatures; the seeded
    // structures are stamped directly through `maps` so they show up in
    // the same maps the next pressure pass reads.
    let empty_bp_map = BlueprintMap::default();
    let empty_pending: AHashMap<BuildSiteKind, u32> = AHashMap::new();
    let civic_gate =
        crate::simulation::organic_settlement::CivicGate::Seed(options.maturity);

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

        // Brain is mandatory for the organic intent path — its parcels
        // / road segments / commons rect / phase drive every site pick.
        // `kickoff_initial_survey_system` runs before this system in the
        // OnEnter chain, so settled non-Paleo factions always have one.
        let brain_ref = settlement_map
            .by_faction
            .get(&faction_id)
            .and_then(|ids| ids.first().copied())
            .and_then(|sid| brains.0.get(&sid));

        // Settlement entity is needed by `append_pressures_for_faction` for
        // peak-population reads. Both the brain lookup and this lookup walk
        // the same `settlement_map.by_faction[fid].first()`.
        let settlement_entity = settlement_map
            .first_for_faction(faction_id)
            .and_then(|sid| settlement_map.by_id.get(&sid).copied());
        let settlement_for_pressures =
            settlement_entity.and_then(|e| settlement_q.get(e).ok());

        // Branch on era. Paleo / Meso keep the band-camp seeder
        // (`paleolithic_hearth_positions_river_aware` provides the canonical
        // multi-hearth layout that the organic pressure path doesn't
        // reproduce — Camp phase has no commons / parcels / road network).
        // Neo+ runs through the unified organic intent loop so seed and
        // runtime emit the same intent stream and obey the same commons /
        // distance-band / road-corridor gates.
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
                // Paleo/Meso band camp — multi-hearth crescent. Every
                // hearth is `Camp` so the population-scaled
                // `ceil(members/6)` pressure formula counts them and the
                // Neolithic+ civic-pressure formula doesn't.
                let role = HearthRole::Camp;
                let e = commands
                    .spawn((
                        Campfire {
                            tier: hearth_tier,
                            role,
                        },
                        StructureLabel(hearth_tier.label()),
                        Transform::from_xyz(world_pos.x, world_pos.y, 0.3),
                        GlobalTransform::default(),
                        Visibility::Visible,
                        InheritedVisibility::default(),
                    ))
                    .id();
                maps.campfire_map
                    .0
                    .insert(hearth_tile, CampfireEntry { entity: e, role });
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
                    faction_id,
                );
            }
            continue;
        }

        // ── Neolithic+: organic pressure → intent → stamp loop ────────
        // Runs the same `append_pressures_for_faction` → `pressure_to_intent`
        // chain the runtime drivers use, but synchronously and with
        // `CivicGate::Seed(maturity)` so civic capital seeds at game start
        // without waiting for `civic_milestone_allows` pop thresholds.
        // Stamping is direct (via `seed_apply_intent`) so members have
        // shelter on tick 0. Each iteration re-reads the live structure
        // maps, so a stamped bed / hearth / granary lowers the next pass's
        // pressure and the loop converges.
        //
        // Cap reasoning: a Bronze 80-pop start needs ~80 huts + 1 of each
        // civic + ~48 palisade tiles ≈ 130 stamps. 512 is comfortable
        // headroom; the stall guard exits earlier in practice.
        let Some(settlement) = settlement_for_pressures else {
            // Settled non-Paleo faction without a Settlement entity is a
            // bug upstream of seeding. Skip rather than fall back —
            // there's no longer a legacy planner to recover into.
            continue;
        };
        let Some(brain) = brain_ref else {
            // Same: `kickoff_initial_survey_system` is in the OnEnter chain
            // ahead of this system. Missing brain ⇒ skip.
            continue;
        };
        const MAX_SEED_ITERATIONS: u32 = 512;
        let mut last_progress_iter: u32 = 0;
        for iter in 0..MAX_SEED_ITERATIONS {
            let mut pressures: Vec<crate::simulation::organic_settlement::SettlementPressure> =
                Vec::new();
            {
                let organic_view = maps.organic_view(&structure_index);
                crate::simulation::organic_settlement::append_pressures_for_faction(
                    faction_id,
                    faction,
                    settlement,
                    &chunk_map,
                    &organic_view,
                    Some(&empty_pending),
                    civic_gate,
                    &mut pressures,
                );
            }
            if pressures.is_empty() {
                break;
            }
            pressures.sort_by(|a, b| {
                b.urgency
                    .partial_cmp(&a.urgency)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });

            let mut applied_any = false;
            let mut chosen_tiles: AHashSet<(i32, i32)> = AHashSet::default();
            // Seed-time RoadField: rebuilt per outer iteration so newly
            // carved roads (intercalated `road_carve_system` runs after the
            // seed pass) feed back in next iteration.
            let road_field = crate::simulation::placement_reachability::road_field_from_home(
                &chunk_map,
                &brain.road_tiles,
                faction.home_tile,
            );
            for pressure in &pressures {
                let intent_opt = {
                    let organic_view = maps.organic_view(&structure_index);
                    crate::simulation::organic_settlement::pressure_to_intent(
                        faction,
                        brain,
                        pressure,
                        &chunk_map,
                        &organic_view,
                        &empty_bp_map,
                        &doormat_reservations,
                        &archetypes,
                        &mut chosen_tiles,
                        civic_gate,
                        &road_field,
                    )
                };
                let Some(intent) = intent_opt else {
                    continue;
                };
                let candidate = build_candidate_from_organic(&intent);
                let tile = candidate.tile;
                let door_dir = candidate.door_dir;
                let hearth_role = intent.hearth_role.unwrap_or(HearthRole::Civic);
                let applied_tile = seed_apply_intent(
                    &mut commands,
                    &mut maps,
                    &mut chunk_map,
                    &mut tile_changed,
                    &mut used,
                    &mut doormat_reservations,
                    &mut road_carve_queue,
                    &mut seed_reservation,
                    &globe,
                    &structure_index,
                    faction_id,
                    home,
                    candidate.intent,
                    tile,
                    door_dir,
                    &seed_techs,
                    Some(brain),
                    hearth_role,
                );
                if applied_tile.is_some() {
                    applied_any = true;
                    last_progress_iter = iter;
                    // Personal kitchen gardens are no longer stamped here
                    // as a seed-time house yard — they are emitted as real
                    // Agricultural parcels by
                    // `organic_settlement::append_dwelling_gardens` and
                    // tilled into `Cropland` through the normal field-prep
                    // pipeline. Cropland only ever appears inside an
                    // Agricultural plot.
                    break;
                }
                // Mark the tile as used so the next iteration's pressure
                // pass can't pick the same blocked anchor again, and so
                // any lower-urgency pressure in this iteration's list
                // skips it via `chosen_tiles`.
                used.insert(tile);
            }

            if !applied_any {
                // Stall guard: 32 consecutive no-progress iterations means
                // every pressure-driven intent is stuck on blocked anchors
                // with no path forward.
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
    interior_hearth: Option<(i32, i32)>,
    wall_material: WallMaterial,
    faction_id: u32,
    home: (i32, i32),
    door_dir: Option<crate::simulation::land::TileEdge>,
    doormat: &mut crate::simulation::doormat::DoormatReservations,
    road_carve: &mut RoadCarveQueue,
    seed_techs: &FactionTechs,
    brain: Option<&crate::simulation::organic_settlement::SettlementBrain>,
) -> bool {
    // L1: civic commons keepout — a walled house may NEVER stamp into the
    // home commons disc. Every upstream picker (`pressure_to_intent` →
    // `choose_site_for_intent`, the relocate spiral) already rejects
    // commons-overlapping footprints; this is the structural backstop so
    // no future caller can ever land here inside the commons regardless
    // of which helper chose the anchor.
    if let Some(b) = brain {
        let foot = crate::simulation::settlement::TileRect::new(
            cx - half_w,
            cy - half_h,
            (2 * half_w + 1) as u16,
            (2 * half_h + 1) as u16,
        );
        if crate::simulation::organic_settlement::rect_intersects_commons(b.commons_rect, foot) {
            return false;
        }
    }
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
        interior_hearth,
    );

    // Simulated-build reachability gate (runs before any wall is stamped, so
    // it sees pre-stamp terrain + the planned wall overlay): doormat connects
    // home and every interior bed is reachable through the door. Refuse the
    // anchor rather than seed a house with a sealed bed.
    if !plan_reachable_from_home(chunk_map, home, planned_doormat, &plan) {
        return false;
    }

    for entry in &plan {
        let tile = entry.tile;
        let kind = &entry.kind;
        let edge = entry.door_edge;
        let world_pos = tile_to_world(tile.0, tile.1);
        match kind {
            BuildSiteKind::EdgeDoor => {
                // Thin housing door on a boundary edge; `tile` is the interior
                // floor tile, doormat is the exterior neighbour. Floor stays
                // passable — no tile rewrite.
                let door_edge = edge.expect("EdgeDoor entry carries its edge");
                let key = outward_edge_key(tile, door_edge);
                let (ddx, ddy) = door_edge.delta();
                let doormat_tile = (tile.0 + ddx, tile.1 + ddy);
                let mid = edge_world_mid(key);
                let e = commands
                    .spawn((
                        EdgeDoorVisual {
                            edge: key,
                            dir: door_edge,
                            open: false,
                        },
                        StructureLabel("Door"),
                        Transform::from_xyz(mid.x, mid.y, 0.4),
                        GlobalTransform::default(),
                        Visibility::Visible,
                        InheritedVisibility::default(),
                    ))
                    .id();
                let ent = maps.edge_structures.0.entry(key).or_default();
                ent.door = Some(EdgeDoorRef {
                    entity: e,
                    open: false,
                    faction_id,
                    dir: door_edge,
                });
                let st = ent.projected_state();
                chunk_map.set_edge_state(key, st);
                doormat.0.insert(
                    doormat_tile,
                    crate::simulation::doormat::DoormatEntry {
                        owner_door: e,
                        door_tile: tile,
                        dir: door_edge,
                    },
                );
                write_road_tile(
                    &mut *chunk_map,
                    &maps.structure_index,
                    tile_changed,
                    doormat_tile,
                );
                queue_door_connector(road_carve, faction_id, doormat_tile, home);
            }
            BuildSiteKind::EdgeWall(mat) => {
                // One perimeter tile owns 1-2 outward boundary edges. Stamp each
                // into `EdgeStructureMap` + the per-chunk edge cache; the floor
                // tile is untouched.
                use crate::simulation::land::TileEdge as TE;
                for (bit, side) in [
                    (edge_side::N, TE::North),
                    (edge_side::E, TE::East),
                    (edge_side::S, TE::South),
                    (edge_side::W, TE::West),
                ] {
                    if entry.edge_sides & bit == 0 {
                        continue;
                    }
                    let key = outward_edge_key(tile, side);
                    let mid = edge_world_mid(key);
                    let e = commands
                        .spawn((
                            EdgeWallVisual {
                                material: *mat,
                                edge: key,
                            },
                            StructureLabel(mat.label()),
                            Transform::from_xyz(mid.x, mid.y, 0.4),
                            GlobalTransform::default(),
                            Visibility::Visible,
                            InheritedVisibility::default(),
                        ))
                        .id();
                    let ent = maps.edge_structures.0.entry(key).or_default();
                    ent.wall = Some(EdgeWall {
                        material: *mat,
                        owner_faction: Some(faction_id),
                        entity: e,
                    });
                    let st = ent.projected_state();
                    chunk_map.set_edge_state(key, st);
                }
            }
            BuildSiteKind::Bed => {
                let bed = Bed {
                    owner: None,
                    tier: best_bed_for(seed_techs),
                    owning_faction: Some(faction_id),
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
            BuildSiteKind::Campfire => {
                // Interior dwelling hearth (Longhouse centre tile). Role
                // comes from the plan entry; for `walled_house_tile_plan`
                // this is always `Domestic` (interior is by definition
                // household). Default-to-Domestic for safety if a future
                // caller forgets to set it.
                let role = entry.hearth_role.unwrap_or(HearthRole::Domestic);
                let tier = SeedConstructionProfile::from_era(current_era(seed_techs)).hearth_tier;
                let e = commands
                    .spawn((
                        Campfire { tier, role },
                        StructureLabel(tier.label()),
                        Transform::from_xyz(world_pos.x, world_pos.y, 0.3),
                        GlobalTransform::default(),
                        Visibility::Visible,
                        InheritedVisibility::default(),
                    ))
                    .id();
                maps.campfire_map
                    .0
                    .insert(tile, CampfireEntry { entity: e, role });
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
        // Nomadic camp — every hearth is `Camp`. Survives pack-and-pitch
        // because role lives on the durable `Campfire` component and
        // `seed_nomadic_camp` is the single re-pitch path.
        let role = HearthRole::Camp;
        let e = commands
            .spawn((
                Campfire {
                    tier: hearth_tier,
                    role,
                },
                StructureLabel(hearth_tier.label()),
                Transform::from_xyz(world_pos.x, world_pos.y, 0.3),
                GlobalTransform::default(),
                Visibility::Visible,
                InheritedVisibility::default(),
            ))
            .id();
        maps.campfire_map
            .0
            .insert(hearth_tile, CampfireEntry { entity: e, role });
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
            faction_id,
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
    faction_id: u32,
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
                let bed = Bed {
                    owner: None,
                    tier: BedTier::Crude,
                    owning_faction: Some(faction_id),
                };
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

/// Lay an **open forager-camp** bedding ring around a Paleolithic/Mesolithic
/// band-camp hearth: a concentric Chebyshev ring of `Bed { tier: Crude }`s.
/// This is the historically-appropriate mobile-band sleeping arrangement and
/// is **distinct from the settled poor-housing path** — it deliberately does
/// NOT route through `organic_settlement::poor_shelter_intent` (lean-tos /
/// sleeping mats), which is reserved for Neolithic+ villages that can't
/// procure wall material. Open camps never emit settled poor shelters.
fn seed_paleo_beds_around_hearth(
    commands: &mut Commands,
    maps: &mut FurnitureMaps,
    chunk_map: &ChunkMap,
    tile_changed: &mut EventWriter<crate::world::chunk_streaming::TileChangedEvent>,
    used: &mut AHashSet<(i32, i32)>,
    hearth: (i32, i32),
    count: u32,
    faction_id: u32,
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
                let bed = Bed {
                    owner: None,
                    tier: BedTier::default(),
                    owning_faction: Some(faction_id),
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

    // ── Wall durability (plans/vehicle-system-tanks.md Phase 1) ───────────

    #[test]
    fn wall_max_hp_scales_with_tier() {
        assert!(WallMaterial::Palisade.max_hp() < WallMaterial::Stone.max_hp());
        assert!(WallMaterial::Stone.max_hp() < WallMaterial::CutStone.max_hp());
    }

    #[test]
    fn sub_resist_hit_still_chips_a_palisade() {
        let mut h = crate::simulation::combat::Health::new(WallMaterial::Palisade.max_hp());
        let dead = apply_wall_damage(&mut h, 5, WallMaterial::Palisade);
        assert!(!dead);
        assert_eq!(h.current, WallMaterial::Palisade.max_hp() - 5);
    }

    #[test]
    fn cutstone_resists_more_than_palisade() {
        let mut cut = crate::simulation::combat::Health::new(100);
        let mut pal = crate::simulation::combat::Health::new(100);
        apply_wall_damage(&mut cut, 20, WallMaterial::CutStone);
        apply_wall_damage(&mut pal, 20, WallMaterial::Palisade);
        assert!(
            cut.current > pal.current,
            "CutStone should take less damage from the same hit"
        );
    }

    #[test]
    fn wall_dies_when_health_drained() {
        let mut h = crate::simulation::combat::Health::new(WallMaterial::Palisade.max_hp());
        let dead = apply_wall_damage(&mut h, 255, WallMaterial::Palisade);
        assert!(dead);
        assert!(h.is_dead());
    }

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
            None,
        );
        // 8 perimeter tiles (thin-wall: one EdgeWall/EdgeDoor per perimeter
        // tile, NOT per edge) + 1 interior bed = 9 entries.
        assert_eq!(plan.len(), 9);
        // Exactly one EdgeDoor, the east-mid perimeter tile (11,10), door East.
        let doors: Vec<_> = plan
            .iter()
            .filter(|p| matches!(p.kind, BuildSiteKind::EdgeDoor))
            .collect();
        assert_eq!(doors.len(), 1);
        assert_eq!(doors[0].tile, (11, 10));
        assert_eq!(doors[0].door_edge, Some(TileEdge::East));
        assert_eq!(doors[0].edge_sides, 0);
        // 7 EdgeWall perimeter tiles. Total outward edges = 12 (4 corners ×2 +
        // 4 mids ×1), minus the door's 1 edge = 11 wall edges across the tiles.
        let walls: Vec<_> = plan
            .iter()
            .filter(|p| matches!(p.kind, BuildSiteKind::EdgeWall(_)))
            .collect();
        assert_eq!(walls.len(), 7);
        let total_wall_edges: u32 = walls.iter().map(|p| p.edge_sides.count_ones()).sum();
        assert_eq!(total_wall_edges, 11);
        // Corners carry two outward sides; mids carry one.
        let corner = walls.iter().find(|p| p.tile == (9, 9)).unwrap(); // SW corner
        assert_eq!(corner.edge_sides, edge_side::S | edge_side::W);
        // Exactly one bed at the interior centre.
        let beds: Vec<_> = plan
            .iter()
            .filter(|p| matches!(p.kind, BuildSiteKind::Bed))
            .collect();
        assert_eq!(beds.len(), 1);
        assert_eq!(beds[0].tile, (10, 10));
    }

    #[test]
    fn plan_reachable_accepts_standard_thin_wall_hut() {
        use crate::world::chunk::{Chunk, ChunkCoord, ChunkMap, CHUNK_SIZE};
        use crate::world::tile::TileKind;
        let mut m = ChunkMap::default();
        let surface_z = Box::new([[0i8; CHUNK_SIZE]; CHUNK_SIZE]);
        let surface_kind = Box::new([[TileKind::Grass; CHUNK_SIZE]; CHUNK_SIZE]);
        let surface_fertility = Box::new([[0u8; CHUNK_SIZE]; CHUNK_SIZE]);
        m.0.insert(
            ChunkCoord(0, 0),
            Chunk::new(surface_z, surface_kind, surface_fertility),
        );
        // 3×3 hut at (10,10), east door (entrance rel (1,0)), one centre bed.
        let plan = walled_house_tile_plan(
            10,
            10,
            1,
            1,
            (1, 0),
            crate::simulation::land::TileEdge::East,
            WallMaterial::Palisade,
            &[(0, 0)],
            None,
        );
        // Doormat is one step east of the east-mid entrance tile (11,10).
        let doormat = (12, 10);
        assert!(
            plan_reachable_from_home(&m, (1, 1), doormat, &plan),
            "a standard thin-wall hut on flat ground must pass the reachability gate"
        );
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
            None,
        );
        let mats: Vec<_> = plan
            .iter()
            .filter_map(|p| match p.kind {
                BuildSiteKind::EdgeWall(m) => Some(m),
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
            None,
        );
        // Perimeter tiles: 5*3 - (3*1 interior) = 15 - 3 = 12.
        // Plus 2 bed entries = 14.
        assert_eq!(plan.len(), 14);
        let door = plan
            .iter()
            .find(|p| matches!(p.kind, BuildSiteKind::EdgeDoor))
            .unwrap();
        assert_eq!(door.tile, (2, 0));
        assert_eq!(door.door_edge, Some(TileEdge::East));
        // Bed cells exactly where requested.
        let bed_tiles: Vec<_> = plan
            .iter()
            .filter_map(|p| match p.kind {
                BuildSiteKind::Bed => Some(p.tile),
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
        // KnowledgeBits equality is structural — same word array means same set.
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
        // Touch the catalog so `core_ids::Thatch.get()` / `Reeds.get()` /
        // `Lime.get()` resolve in test paths that bypass `WorldPlugin`.
        let _ = core_ids::catalog();
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
        // Neolithic top rung = Mudbrick (stone 2 + wood 1 + thatch 1 after
        // Phase F.2 recipe split — thatch is the straw binder).
        let t = neo_techs();
        assert_eq!(best_wall_material(&t), WallMaterial::Mudbrick);
        let thatch = *core_ids::Thatch
            .get()
            .expect("core_ids: thatch not initialised");
        let v = view_with(&[
            (core_ids::stone(), Scarcity::Available),
            (core_ids::wood(), Scarcity::Available),
            (thatch, Scarcity::Available),
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
        let thatch = *core_ids::Thatch
            .get()
            .expect("core_ids: thatch not initialised");
        let v = view_with(&[
            (core_ids::stone(), Scarcity::Scarce),
            (core_ids::wood(), Scarcity::Available),
            (thatch, Scarcity::Available),
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
        // Mudbrick needs stone; mark stone Unavailable but WattleDaub's
        // inputs (wood + reeds after Phase F.2 recipe split) available →
        // step down to WattleDaub (not emergency).
        let t = neo_techs();
        let reeds = *core_ids::Reeds
            .get()
            .expect("core_ids: reeds not initialised");
        let v = view_with(&[
            (core_ids::stone(), Scarcity::Unavailable),
            (core_ids::wood(), Scarcity::Available),
            (reeds, Scarcity::Available),
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

    // ── Poor-housing (plans/realistic-poor-shelter.md) ───────────────────

    fn poor_ids() -> (
        crate::economy::resource_catalog::ResourceId,
        crate::economy::resource_catalog::ResourceId,
        crate::economy::resource_catalog::ResourceId,
        crate::economy::resource_catalog::ResourceId,
    ) {
        let _ = core_ids::catalog();
        (
            *core_ids::Reeds.get().expect("reeds"),
            *core_ids::Thatch.get().expect("thatch"),
            core_ids::wood(),
            core_ids::skin(),
        )
    }

    #[test]
    fn poor_shelter_material_ladder_reed_thatch_brush_bareground() {
        let (reeds, thatch, wood, skin) = poor_ids();

        // Reeds available → reed screen + reed mat.
        let v = view_with(&[(reeds, Scarcity::Available)]);
        let s = select_poor_shelter_material(Some(&v));
        assert_eq!(s.shelter, Some(LightShelterMaterial::ReedScreen));
        assert_eq!(s.mat, SleepingMatMaterial::Reed);

        // Reeds gone, thatch + wood available → thatch lean-to + thatch mat.
        let v = view_with(&[(thatch, Scarcity::Available), (wood, Scarcity::Available)]);
        let s = select_poor_shelter_material(Some(&v));
        assert_eq!(s.shelter, Some(LightShelterMaterial::ThatchLeanTo));
        assert_eq!(s.mat, SleepingMatMaterial::Thatch);

        // Only wood → brush lean-to; no fibre → hide if available else bare.
        let v = view_with(&[(wood, Scarcity::Available), (skin, Scarcity::Available)]);
        let s = select_poor_shelter_material(Some(&v));
        assert_eq!(s.shelter, Some(LightShelterMaterial::BrushLeanTo));
        assert_eq!(s.mat, SleepingMatMaterial::Hide);

        // Everything unavailable → no shelter, bare-ground mat (last resort).
        let v = MaterialAvailabilityView::default();
        let s = select_poor_shelter_material(Some(&v));
        assert_eq!(s.shelter, None);
        assert_eq!(s.mat, SleepingMatMaterial::BareGround);
    }

    #[test]
    fn poor_shelter_scarce_input_still_buildable() {
        // A `Scarce` (market-procurable) input must still count as buildable —
        // mirrors `select_wall_material` treating Scarce as not-Unavailable.
        let (reeds, ..) = poor_ids();
        let v = view_with(&[(reeds, Scarcity::Scarce)]);
        let s = select_poor_shelter_material(Some(&v));
        assert_eq!(s.shelter, Some(LightShelterMaterial::ReedScreen));
    }

    #[test]
    fn sleeping_mat_recovery_mult_is_graduated() {
        assert_eq!(BedTier::SleepingMat.sleep_recovery_mult(), 1.25);
        assert_eq!(BedTier::Crude.sleep_recovery_mult(), 2.0);
        assert_eq!(BedTier::Framed.sleep_recovery_mult(), 2.0);
        assert_eq!(BedTier::Carved.sleep_recovery_mult(), 2.0);
    }

    #[test]
    fn best_bed_never_returns_sleeping_mat() {
        // SleepingMat is emergency-only; the tech-driven picker must never
        // resolve to it regardless of tech level.
        for era in [
            Era::Paleolithic,
            Era::Mesolithic,
            Era::Neolithic,
            Era::Chalcolithic,
            Era::BronzeAge,
        ] {
            assert_ne!(best_bed_for(&techs_through_era(era)), BedTier::SleepingMat);
        }
    }

    #[test]
    fn shelter_tier_relief_below_one_enclosure_point() {
        // Every lightweight shelter must give less relief than a single
        // enclosure point so a walled house stays strictly better.
        for tier in [ShelterTier::LeanTo, ShelterTier::Tent, ShelterTier::Yurt] {
            assert!(tier.relief_per_day() < 27.0, "{:?}", tier);
        }
        // Ordering: lean-to < tent < yurt.
        assert!(ShelterTier::LeanTo.relief_per_day() < ShelterTier::Tent.relief_per_day());
        assert!(ShelterTier::Tent.relief_per_day() < ShelterTier::Yurt.relief_per_day());
    }

    #[test]
    fn shelter_map_strongest_covering_picks_strongest_in_radius() {
        let mut m = ShelterMap::default();
        // Lean-to directly under the agent (radius 0).
        m.0.insert(
            (0, 0),
            ShelterEntry {
                entity: Entity::from_raw(1),
                tier: ShelterTier::LeanTo,
            },
        );
        assert_eq!(m.strongest_covering((0, 0)), Some(ShelterTier::LeanTo));
        // A yurt one tile away (radius 1) covers the agent and outranks the
        // lean-to.
        m.0.insert(
            (1, 0),
            ShelterEntry {
                entity: Entity::from_raw(2),
                tier: ShelterTier::Yurt,
            },
        );
        assert_eq!(m.strongest_covering((0, 0)), Some(ShelterTier::Yurt));
        // A lean-to (radius 0) two tiles away does NOT cover the agent.
        let mut m2 = ShelterMap::default();
        m2.0.insert(
            (2, 0),
            ShelterEntry {
                entity: Entity::from_raw(3),
                tier: ShelterTier::LeanTo,
            },
        );
        assert_eq!(m2.strongest_covering((0, 0)), None);
    }

    // ── Settlement realism: door connector (cardinal) ─────────────────

    fn flat_grass_map() -> ChunkMap {
        use crate::world::chunk::{Chunk, ChunkCoord, CHUNK_SIZE};
        let mut cm = ChunkMap::default();
        for cy in -1..=1 {
            for cx in -1..=1 {
                let z = Box::new([[4i8; CHUNK_SIZE]; CHUNK_SIZE]);
                let kind = Box::new([[TileKind::Grass; CHUNK_SIZE]; CHUNK_SIZE]);
                let fert = Box::new([[0u8; CHUNK_SIZE]; CHUNK_SIZE]);
                cm.0.insert(ChunkCoord(cx, cy), Chunk::new(z, kind, fert));
            }
        }
        cm
    }

    fn set_tile_kind(cm: &mut ChunkMap, x: i32, y: i32, kind: TileKind) {
        cm.set_tile(
            x,
            y,
            4,
            TileData {
                kind,
                ..Default::default()
            },
        );
    }

    #[test]
    fn door_connector_already_connected_when_cardinal_road_adjacent() {
        let mut cm = flat_grass_map();
        // Road tile cardinally east of doormat=(5,5).
        set_tile_kind(&mut cm, 6, 5, TileKind::Road);
        let plan = find_door_connector(&cm, None, (0, 0), (5, 5), 12, |_| false);
        assert_eq!(
            plan,
            DoorConnectorPlan::AlreadyConnected,
            "a cardinal-adjacent road ⇒ already connected"
        );
    }

    #[test]
    fn door_connector_diagonal_only_road_requires_connector() {
        let mut cm = flat_grass_map();
        // Road tile only diagonally adjacent to doormat=(5,5).
        set_tile_kind(&mut cm, 6, 6, TileKind::Road);
        let plan = find_door_connector(&cm, None, (0, 0), (5, 5), 12, |_| false);
        match plan {
            DoorConnectorPlan::Connector(path) => {
                assert!(!path.is_empty(), "diagonal-only road ⇒ a real connector");
                // The last carved tile must be cardinally adjacent to the road.
                let last = *path.last().unwrap();
                let cardinal_to_road = [(1, 0), (-1, 0), (0, 1), (0, -1)]
                    .iter()
                    .any(|(dx, dy)| {
                        cm.tile_kind_at(last.0 + dx, last.1 + dy) == Some(TileKind::Road)
                    });
                assert!(cardinal_to_road, "connector must end cardinally on the road");
            }
            other => panic!("diagonal-only road should need a connector; got {:?}", other),
        }
    }

    #[test]
    fn door_connector_routes_around_blocked_and_farm_tiles() {
        let mut cm = flat_grass_map();
        // Road to the north; a structure at (5,7) and Cropland at (5,8) sit on
        // the straight-line path and must be routed around.
        set_tile_kind(&mut cm, 5, 10, TileKind::Road);
        set_tile_kind(&mut cm, 5, 8, TileKind::Cropland);
        let blocked = |t: (i32, i32)| t == (5, 7);
        let plan = find_door_connector(&cm, None, (0, 0), (5, 5), 12, blocked);
        match plan {
            DoorConnectorPlan::Connector(path) => {
                assert!(!path.is_empty());
                assert!(!path.contains(&(5, 7)), "must not carve the blocked tile");
                assert!(!path.contains(&(5, 8)), "must not carve the Cropland tile");
            }
            other => panic!("expected a routed connector; got {:?}", other),
        }
    }

    #[test]
    fn door_connector_falls_back_to_home_when_no_spine() {
        let cm = flat_grass_map();
        // No roads, no brain ⇒ HomeFallback toward a reachable home tile.
        let plan = find_door_connector(&cm, None, (5, 0), (5, 5), 12, |_| false);
        match plan {
            DoorConnectorPlan::HomeFallback(path) => {
                assert!(!path.is_empty(), "open grass ⇒ a cardinal path toward home");
            }
            other => panic!("no spine ⇒ home fallback; got {:?}", other),
        }
    }

    #[test]
    fn road_width_is_tier_and_era_aware() {
        use crate::simulation::settlement::StreetTier;
        use crate::simulation::technology::Era;
        assert_eq!(road_width_for(StreetTier::Alley, Era::BronzeAge), 1);
        assert_eq!(road_width_for(StreetTier::Secondary, Era::BronzeAge), 2);
        assert_eq!(road_width_for(StreetTier::Primary, Era::Neolithic), 2);
        assert_eq!(road_width_for(StreetTier::Primary, Era::BronzeAge), 3);
    }

    // ── Settlement realism: civic-seeding maturity ────────────────────

    #[test]
    fn should_seed_civic_founder_skips_market_for_small_neolithic() {
        use crate::game_state::StartSettlementMaturity;
        use crate::simulation::civic_milestones::{should_seed_civic, CivicKind};
        // Founder + Neolithic 20-pop ⇒ Market gated out.
        assert!(!should_seed_civic(
            CivicKind::Market,
            Era::Neolithic,
            20,
            StartSettlementMaturity::Founder,
            true,
        ));
        // Established ⇒ seeds.
        assert!(should_seed_civic(
            CivicKind::Market,
            Era::Neolithic,
            20,
            StartSettlementMaturity::Established,
            true,
        ));
    }

    // ── restamp_walls_on_chunk_load ─────────────────────────────────────
    //
    // A constructed wall writes `TileKind::Wall` as a chunk delta; chunk
    // regen on stream-in loses the delta while the `Wall` entity in
    // `WallMap` persists. The restamp re-projects the tile so a reloaded
    // built wall stays impassable.

    use crate::world::chunk::Chunk;

    type ChunkLoadedEvent = crate::world::chunk_streaming::ChunkLoadedEvent;
    type TileChangedEvent = crate::world::chunk_streaming::TileChangedEvent;

    /// `App` with the wall restamp system + one freshly-regenerated chunk
    /// at (0,0) whose surface is `surf_kind` at `surf_z`.
    fn wall_restamp_harness(surf_kind: TileKind, surf_z: i8) -> App {
        let mut app = App::new();
        app.add_event::<ChunkLoadedEvent>()
            .add_event::<TileChangedEvent>()
            .insert_resource(WallMap::default());

        let mut chunk_map = ChunkMap::default();
        let z = Box::new([[surf_z; CHUNK_SIZE]; CHUNK_SIZE]);
        let kind = Box::new([[surf_kind; CHUNK_SIZE]; CHUNK_SIZE]);
        let fert = Box::new([[0u8; CHUNK_SIZE]; CHUNK_SIZE]);
        chunk_map
            .0
            .insert(ChunkCoord(0, 0), Chunk::new(z, kind, fert));
        app.insert_resource(chunk_map);

        app.add_systems(Update, restamp_walls_on_chunk_load);
        app
    }

    fn drain_wall_changed(app: &mut App) -> Vec<(i32, i32)> {
        app.world_mut()
            .resource_mut::<Events<TileChangedEvent>>()
            .drain()
            .map(|e| (e.tx, e.ty))
            .collect()
    }

    #[test]
    fn reverted_constructed_wall_is_restamped() {
        // Regenerated chunk shows natural Grass; the durable Wall entity
        // persisted in WallMap (the chunk-delta-not-reapplied gap).
        let mut app = wall_restamp_harness(TileKind::Grass, 3);
        let e = app.world_mut().spawn_empty().id();
        app.world_mut().resource_mut::<WallMap>().0.insert((5, 3), e);

        app.world_mut()
            .resource_mut::<Events<ChunkLoadedEvent>>()
            .send(ChunkLoadedEvent {
                coord: ChunkCoord(0, 0),
            });
        app.update();

        assert_eq!(
            app.world().resource::<ChunkMap>().tile_kind_at(5, 3),
            Some(TileKind::Wall),
            "wall tile must be re-stamped from WallMap on reload"
        );
        assert!(
            drain_wall_changed(&mut app).contains(&(5, 3)),
            "restamp must emit TileChangedEvent so pathing/sprites rebuild"
        );

        // Second load (chunk already correct) is a no-op — no event churn.
        app.world_mut()
            .resource_mut::<Events<ChunkLoadedEvent>>()
            .send(ChunkLoadedEvent {
                coord: ChunkCoord(0, 0),
            });
        app.update();
        assert!(drain_wall_changed(&mut app).is_empty());
    }

    #[test]
    fn natural_wall_restamp_is_idempotent() {
        // Natural exposed bedrock regenerates as Wall on its own — the
        // restamp must skip it and emit nothing.
        let mut app = wall_restamp_harness(TileKind::Wall, 4);
        let e = app.world_mut().spawn_empty().id();
        app.world_mut().resource_mut::<WallMap>().0.insert((2, 6), e);

        app.world_mut()
            .resource_mut::<Events<ChunkLoadedEvent>>()
            .send(ChunkLoadedEvent {
                coord: ChunkCoord(0, 0),
            });
        app.update();

        assert!(
            drain_wall_changed(&mut app).is_empty(),
            "already-Wall tile must not be restamped"
        );
    }

    #[test]
    fn wall_restamp_skips_chunks_that_did_not_load() {
        // A WallMap entry in a chunk that fired no ChunkLoadedEvent must
        // be left untouched.
        let mut app = wall_restamp_harness(TileKind::Grass, 3);
        let e = app.world_mut().spawn_empty().id();
        app.world_mut().resource_mut::<WallMap>().0.insert((5, 3), e);

        // No ChunkLoadedEvent sent.
        app.update();

        assert_eq!(
            app.world().resource::<ChunkMap>().tile_kind_at(5, 3),
            Some(TileKind::Grass),
            "tile in an unloaded chunk must not be restamped"
        );
        assert!(drain_wall_changed(&mut app).is_empty());
    }

    // ── Bed eligibility predicate (plans/workers-sleeping-outside.md) ────────

    /// Identity root for tests with no parent-faction graph.
    fn root_identity(fid: u32) -> u32 {
        fid
    }
    fn no_plot(_tile: (i32, i32)) -> Option<u32> {
        None
    }

    #[test]
    fn same_faction_tagged_bed_beyond_30_tiles_is_eligible() {
        // Pre-fix: any bed >30 chebyshev from home_tile was rejected. Now the
        // faction tag short-circuits the radius gate so a satellite Hamlet bed
        // 40 tiles away is claimable by its own faction.
        let anchors = vec![(0, 0)];
        assert!(bed_eligible_for_faction(
            Some(7),
            (40, 0),
            7,
            &root_identity,
            &no_plot,
            &anchors,
        ));
    }

    #[test]
    fn other_faction_tagged_bed_is_rejected() {
        // Tagged-bed mismatch wins over any radius proximity.
        let anchors = vec![(0, 0)];
        assert!(!bed_eligible_for_faction(
            Some(9),
            (5, 5),
            7,
            &root_identity,
            &no_plot,
            &anchors,
        ));
    }

    #[test]
    fn legacy_untagged_bed_in_same_faction_plot_is_eligible() {
        // No tag, but plot lookup returns viewer's own faction → claim.
        let anchors = vec![(0, 0)];
        let plot_of = |tile: (i32, i32)| if tile == (50, 50) { Some(7) } else { None };
        assert!(bed_eligible_for_faction(
            None,
            (50, 50),
            7,
            &root_identity,
            &plot_of,
            &anchors,
        ));
    }

    #[test]
    fn legacy_untagged_bed_in_other_faction_plot_is_rejected() {
        // Plot ownership trumps the radius backstop — a foreign plot blocks the
        // claim even if the bed sits inside `BED_FALLBACK_RADIUS` of home.
        let anchors = vec![(0, 0)];
        let plot_of = |tile: (i32, i32)| if tile == (10, 10) { Some(9) } else { None };
        assert!(!bed_eligible_for_faction(
            None,
            (10, 10),
            7,
            &root_identity,
            &plot_of,
            &anchors,
        ));
    }

    #[test]
    fn legacy_untagged_bed_without_plot_uses_anchor_radius() {
        // No tag, no plot — fall through to the chebyshev radius backstop.
        // Inside radius around any anchor → eligible.
        let anchors = vec![(0, 0), (200, 200)];
        assert!(bed_eligible_for_faction(
            None,
            (15, 25),
            7,
            &root_identity,
            &no_plot,
            &anchors,
        ));
        // Settlement-anchor backstop catches beds near a satellite market_tile.
        assert!(bed_eligible_for_faction(
            None,
            (220, 215),
            7,
            &root_identity,
            &no_plot,
            &anchors,
        ));
    }

    #[test]
    fn legacy_untagged_bed_outside_all_anchors_is_rejected() {
        let anchors = vec![(0, 0), (200, 200)];
        assert!(!bed_eligible_for_faction(
            None,
            (1000, 1000),
            7,
            &root_identity,
            &no_plot,
            &anchors,
        ));
    }

    #[test]
    fn household_member_claims_parent_village_bed_via_root() {
        // Household 12 is a sub-faction of village 7. Bed is tagged for the
        // village; household member's viewer_root resolves to 7 → eligible.
        let anchors = vec![(0, 0)];
        let root_of = |fid: u32| match fid {
            12 => 7,
            other => other,
        };
        // Worker is in household 12 (viewer_root = root_of(12) = 7).
        let viewer_root = root_of(12);
        assert!(bed_eligible_for_faction(
            Some(7),
            (5, 5),
            viewer_root,
            &root_of,
            &no_plot,
            &anchors,
        ));
        // Bed tagged for sibling household 13 (also rooted at 7) still claimable.
        let root_of2 = |fid: u32| match fid {
            12 | 13 => 7,
            other => other,
        };
        assert!(bed_eligible_for_faction(
            Some(13),
            (5, 5),
            viewer_root,
            &root_of2,
            &no_plot,
            &anchors,
        ));
    }

    #[test]
    fn chebyshev_backstop_uses_30_tile_radius() {
        // Boundary check on `BED_FALLBACK_RADIUS = 30`.
        let anchors = vec![(0, 0)];
        // Exactly 30 chebyshev → eligible.
        assert!(bed_eligible_for_faction(
            None,
            (30, 0),
            7,
            &root_identity,
            &no_plot,
            &anchors,
        ));
        assert!(bed_eligible_for_faction(
            None,
            (30, 30),
            7,
            &root_identity,
            &no_plot,
            &anchors,
        ));
        // 31 chebyshev → rejected.
        assert!(!bed_eligible_for_faction(
            None,
            (31, 0),
            7,
            &root_identity,
            &no_plot,
            &anchors,
        ));
    }

    // ───────────── Bed-claim reconciliation predicates ─────────────

    #[test]
    fn bed_owner_cleared_when_owner_entity_gone() {
        let bed = Entity::from_raw(1);
        // Dead/despawned owner → clear regardless of the other inputs.
        assert!(bed_owner_is_stale(bed, false, Some(bed), true));
    }

    #[test]
    fn bed_owner_cleared_when_claim_not_reciprocal() {
        let bed = Entity::from_raw(1);
        let other_bed = Entity::from_raw(2);
        // Owner alive + eligible but its HomeBed points elsewhere (or nowhere).
        assert!(bed_owner_is_stale(bed, true, Some(other_bed), true));
        assert!(bed_owner_is_stale(bed, true, None, true));
    }

    #[test]
    fn bed_owner_cleared_when_no_longer_eligible() {
        let bed = Entity::from_raw(1);
        // Reciprocal claim but bed no longer eligible for the owner's root.
        assert!(bed_owner_is_stale(bed, true, Some(bed), false));
    }

    #[test]
    fn bed_owner_retained_when_live_reciprocal_eligible() {
        let bed = Entity::from_raw(1);
        assert!(!bed_owner_is_stale(bed, true, Some(bed), true));
    }

    #[test]
    fn home_bed_claim_stale_when_missing_or_dead() {
        let p = Entity::from_raw(10);
        let bed = Entity::from_raw(1);
        // No claim.
        assert!(home_bed_claim_is_stale(p, None, false, None, false));
        // Claim points at a despawned bed.
        assert!(home_bed_claim_is_stale(p, Some(bed), false, None, false));
    }

    #[test]
    fn home_bed_claim_stale_when_owner_mismatch_or_ineligible() {
        let p = Entity::from_raw(10);
        let other = Entity::from_raw(11);
        let bed = Entity::from_raw(1);
        // Phantom claim: bed owned by someone else.
        assert!(home_bed_claim_is_stale(p, Some(bed), true, Some(other), true));
        // Phantom claim: bed unowned.
        assert!(home_bed_claim_is_stale(p, Some(bed), true, None, true));
        // Reciprocal but ineligible (e.g. plot changed hands).
        assert!(home_bed_claim_is_stale(p, Some(bed), true, Some(p), false));
    }

    #[test]
    fn home_bed_claim_fresh_when_reciprocal_and_eligible() {
        let p = Entity::from_raw(10);
        let bed = Entity::from_raw(1);
        assert!(!home_bed_claim_is_stale(p, Some(bed), true, Some(p), true));
    }

    #[test]
    fn reroute_when_valid_claim_off_bed_and_no_recent_failure() {
        assert!(should_reroute_bedless_sleeper(true, false, false));
    }

    #[test]
    fn no_reroute_without_valid_claim() {
        assert!(!should_reroute_bedless_sleeper(false, false, false));
    }

    #[test]
    fn no_reroute_when_already_on_bed() {
        assert!(!should_reroute_bedless_sleeper(true, true, false));
    }

    #[test]
    fn no_reroute_during_sleep_route_failure_cooldown() {
        // Guard against per-pass churn against a genuinely unreachable bed.
        assert!(!should_reroute_bedless_sleeper(true, false, true));
    }
}
