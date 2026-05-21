//! Physically-excavated wells.
//!
//! A well is no longer a 1-tile entity with a virtual `WELL_REACH_Z` water
//! gate. It is a staged structure: workers **excavate a navigable shaft** down
//! to the aquifer table, **line its rim with constructed walls**, and the
//! finished well holds a **physical water column** (`RuntimeWater`) drawn from
//! the groundwater the dig broke into.
//!
//! ## Geometry — a 5×5 stepwell
//!
//! For a well centred on `(cx, cy)` with surface `surf_z`:
//! - **Centre column** `(cx, cy)` — carved straight down to `bottom_z`, left
//!   open (Air → Water). The impassable water shaft; agents draw from a
//!   chebyshev-adjacent tile.
//! - **Inner ring** (8 tiles) — a one-turn descending helix. Ring tile *k*
//!   (clockwise from N, k = 1..=depth) is carved to `surf_z - k` and stamped
//!   `TileKind::Ramp`, so `|Δz| ≤ 1` graph edges connect the spiral with no
//!   pathfinding change. Eight ring tiles ⇒ a hard depth cap of 8 Z.
//! - **Outer ring** (5×5 perimeter, 16 tiles) — a rim parapet of constructed
//!   `Wall` blueprints, minus a one-tile gateway aligned with the helix entry
//!   so workers and drinkers can reach the pit.
//!
//! See `plans/dynamic-wells.md` (superseded) and the mossy-snuggling-puddle
//! plan for the full design.

use ahash::{AHashMap, AHashSet};
use bevy::prelude::*;

use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_HEIGHT, CHUNK_SIZE, Z_MIN};
use crate::world::globe::Globe;
use crate::world::terrain::GLOBE_H_TO_Z;

/// A hand-dug well shaft reaches at most this many Z below the surface. The
/// 8-tile inner ring provides exactly 8 Z of stepped descent in one
/// non-overlapping helix turn; a deeper aquifer cannot host a hand-dug well.
pub const MAX_HAND_DUG_WELL_DEPTH_Z: i32 = 8;
/// Even over a high water table a well gets a real little shaft.
pub const MIN_WELL_DEPTH_Z: i32 = 2;
/// The shaft bottoms out one Z below the water table so a sump of standing
/// water collects.
pub const WELL_SUMP_Z: i32 = 1;

/// Per-cell water-table Z, in the same frame as `ChunkMap::surface_z_at` and
/// `water_runtime::aquifer_seep_emitter_system`: anchor on the jitter-free
/// macro elevation, subtract the aquifer depth. `None` when the climate /
/// hydrology cell is unresolvable. Factored out of the legacy `well_has_water`
/// block so the well dig, the seep sim, and the drink check share one formula.
pub fn aquifer_z_at(globe: &Globe, tile: (i32, i32)) -> Option<f32> {
    let hc = globe.hydro_cell_at(tile.0, tile.1)?;
    let (elev_u, _, _) = globe.sample_climate(tile.0, tile.1);
    let macro_f = (elev_u / 255.0).clamp(0.0, 1.0);
    let cell_surface_z = Z_MIN as f32 + macro_f * CHUNK_HEIGHT as f32;
    let aquifer_depth_z = (hc.filled_height - hc.aquifer_level) * GLOBE_H_TO_Z;
    Some(cell_surface_z - aquifer_depth_z)
}

/// Resolved excavation spec for a candidate well tile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WellSpec {
    pub surf_z: i8,
    pub bottom_z: i8,
    /// Z-levels from surface to shaft bottom (`surf_z - bottom_z`); also the
    /// helix length in ring tiles.
    pub depth: i32,
}

/// Outcome of resolving a well site against the aquifer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WellResult {
    Ok(WellSpec),
    /// Water table deeper than a hand-dug shaft can reach — site rejected.
    TooDeep,
    /// Chunk / hydrology not loaded — caller defers or uses a default.
    Unresolvable,
}

/// Pure depth derivation. `surf_z` is the well-tile surface; `table_z` the
/// per-cell aquifer Z from [`aquifer_z_at`].
pub fn well_spec_from(surf_z: i32, table_z: f32) -> WellResult {
    let needed = surf_z - table_z.floor() as i32 + WELL_SUMP_Z;
    if needed > MAX_HAND_DUG_WELL_DEPTH_Z {
        return WellResult::TooDeep;
    }
    let depth = needed.clamp(MIN_WELL_DEPTH_Z, MAX_HAND_DUG_WELL_DEPTH_Z);
    let surf_z_i8 = surf_z.clamp(i8::MIN as i32, i8::MAX as i32) as i8;
    let bottom_z = (surf_z - depth).clamp(Z_MIN, i8::MAX as i32) as i8;
    WellResult::Ok(WellSpec {
        surf_z: surf_z_i8,
        bottom_z,
        depth,
    })
}

/// Resolve a well site at `center` against the live world.
pub fn well_spec_at(globe: &Globe, chunk_map: &ChunkMap, center: (i32, i32)) -> WellResult {
    let surf_z = chunk_map.surface_z_at(center.0, center.1);
    if surf_z < Z_MIN {
        return WellResult::Unresolvable;
    }
    match aquifer_z_at(globe, center) {
        Some(table_z) => well_spec_from(surf_z, table_z),
        None => WellResult::Unresolvable,
    }
}

/// The 8 inner-ring tiles, clockwise from North. Consecutive tiles are
/// chebyshev-adjacent, so ring tile *k* (Z `surf_z - k`) and ring tile *k+1*
/// (Z `surf_z - k - 1`) form a walkable `|Δz| = 1` step — the descent helix.
pub fn inner_ring(center: (i32, i32)) -> [(i32, i32); 8] {
    const OFFSETS: [(i32, i32); 8] = [
        (0, -1),
        (1, -1),
        (1, 0),
        (1, 1),
        (0, 1),
        (-1, 1),
        (-1, 0),
        (-1, -1),
    ];
    OFFSETS.map(|(dx, dy)| (center.0 + dx, center.1 + dy))
}

/// The 16 outer-ring tiles — the 5×5 perimeter. These carry the lining-wall
/// parapet (minus the gateway, see [`gateway_tile`]).
pub fn outer_ring(center: (i32, i32)) -> Vec<(i32, i32)> {
    let mut tiles = Vec::with_capacity(16);
    for dy in -2..=2i32 {
        for dx in -2..=2i32 {
            if dx.abs() == 2 || dy.abs() == 2 {
                tiles.push((center.0 + dx, center.1 + dy));
            }
        }
    }
    tiles
}

/// The single outer-ring tile left unwalled so workers and drinkers can enter
/// the pit. Sits due North — directly out from inner-ring tile 1 (the helix
/// entry), at `(cx, cy - 2)`.
pub fn gateway_tile(center: (i32, i32)) -> (i32, i32) {
    (center.0, center.1 - 2)
}

/// Every tile the excavation phase must carve, with its per-tile target Z:
/// the centre column to `bottom_z`, then helix ring tiles 1..=depth.
pub fn excavation_targets(spec: &WellSpec, center: (i32, i32)) -> Vec<((i32, i32), i8)> {
    let mut out = Vec::with_capacity(1 + spec.depth as usize);
    out.push((center, spec.bottom_z));
    let ring = inner_ring(center);
    for k in 1..=spec.depth {
        let tile = ring[(k - 1) as usize];
        out.push((tile, (spec.surf_z as i32 - k) as i8));
    }
    out
}

/// Every tile of the 5×5 stepwell footprint (centre + inner helix + outer
/// lining ring). Used by placement validation to reject a well whose footprint
/// would overlap an existing structure.
pub fn well_footprint(center: (i32, i32)) -> Vec<(i32, i32)> {
    let mut tiles = Vec::with_capacity(25);
    for dy in -2..=2i32 {
        for dx in -2..=2i32 {
            tiles.push((center.0 + dx, center.1 + dy));
        }
    }
    tiles
}

/// Reconstruct the excavation [`WellSpec`] from a finished [`Well`]. `Well`
/// stores `surf_z` + `bottom_z`; the helix length is exactly their difference.
pub fn well_spec_of(well: &Well) -> WellSpec {
    WellSpec {
        surf_z: well.surf_z,
        bottom_z: well.bottom_z,
        depth: well.surf_z as i32 - well.bottom_z as i32,
    }
}

/// Construction phase of an in-progress well.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WellPhase {
    /// Workers carving the shaft + helix down to the water table.
    Excavating,
    /// Shaft dug; rim lining walls under construction.
    Lining,
    /// Lining done; spawn the wellhead next progression tick.
    Capping,
}

/// Depth (Z-units) of standing water charged into a freshly-dug shaft — the
/// dig broke into the saturated zone, so water arrives at once rather than
/// over the ~140-day pure-seep timescale. The fluid sim caps it at the table.
pub const WELL_INITIAL_CHARGE_Z: f32 = 1.5;
/// A well yields water while its shaft column is at least this deep.
pub const WELL_MIN_DRINKABLE_DEPTH: f32 = 0.3;
/// Water column drawn down per drink sip.
pub const WELL_SIP_DRAWDOWN_Z: f32 = 0.02;

/// An in-progress well. The durable truth for the multi-phase build; converted
/// from a placed `BuildSiteKind::Well` blueprint and resolved into a finished
/// `Well` entity at the end of `Capping`.
#[derive(Component, Clone, Debug)]
pub struct WellSite {
    pub faction_id: u32,
    pub center: (i32, i32),
    pub surf_z: i8,
    pub bottom_z: i8,
    pub depth: i32,
    pub phase: WellPhase,
    pub author: Option<crate::simulation::construction::BlueprintAuthor>,
    /// Outer-ring lining-wall tiles spawned on the first progression tick;
    /// `Lining` completes once none remain in `BlueprintMap`.
    pub lining_tiles: Vec<(i32, i32)>,
    /// Set once the lining-wall blueprints have been spawned (idempotency).
    pub walls_spawned: bool,
}

impl WellSite {
    pub fn new(
        faction_id: u32,
        center: (i32, i32),
        spec: WellSpec,
        author: Option<crate::simulation::construction::BlueprintAuthor>,
    ) -> Self {
        Self {
            faction_id,
            center,
            surf_z: spec.surf_z,
            bottom_z: spec.bottom_z,
            depth: spec.depth,
            phase: WellPhase::Excavating,
            author,
            lining_tiles: Vec::new(),
            walls_spawned: false,
        }
    }

    pub fn spec(&self) -> WellSpec {
        WellSpec {
            surf_z: self.surf_z,
            bottom_z: self.bottom_z,
            depth: self.depth,
        }
    }
}

/// Centre tile → `WellSite` entity. Indexes in-progress wells.
#[derive(Resource, Default)]
pub struct WellSiteMap(pub AHashMap<(i32, i32), Entity>);

// ---------------------------------------------------------------------------
// Staged construction
// ---------------------------------------------------------------------------

use crate::simulation::carve::carve_tile;
use crate::simulation::construction::{
    best_wall_material, Blueprint, BlueprintAuthor, BlueprintMap, BuildSiteKind, StructureIndex,
    StructureLabel, Well, WellMap,
};
use crate::simulation::faction::{FactionRegistry, FactionTechs};
use crate::simulation::terraform::{TerraformMap, TerraformSite};
use crate::world::chunk_streaming::{ChunkLoadedEvent, TileChangedEvent};
use crate::world::terrain::{tile_to_world, WorldGen};
use crate::world::water_runtime::{RuntimeWater, RuntimeWaterCell, AQUIFER_SEEP_RATE};

/// Spawn a staged `WellSite` and its excavation `TerraformSite`s. Used by every
/// placement path (manual build, chief/organic, conversion of a placed
/// `BuildSiteKind::Well` blueprint). Returns the `WellSite` entity.
pub fn spawn_well_site(
    commands: &mut Commands,
    well_site_map: &mut WellSiteMap,
    terraform_map: &mut TerraformMap,
    faction_id: u32,
    center: (i32, i32),
    spec: WellSpec,
    author: Option<BlueprintAuthor>,
) -> Entity {
    let wp = tile_to_world(center.0, center.1);
    let site = commands
        .spawn((
            WellSite::new(faction_id, center, spec, author),
            Transform::from_xyz(wp.x, wp.y, 0.35),
            GlobalTransform::default(),
            Visibility::Visible,
            InheritedVisibility::default(),
        ))
        .id();
    well_site_map.0.insert(center, site);

    // One TerraformSite per descending footprint tile (centre + helix ring).
    for (tile, target_z) in excavation_targets(&spec, center) {
        if terraform_map.0.contains_key(&tile) {
            continue;
        }
        let twp = tile_to_world(tile.0, tile.1);
        let e = commands
            .spawn((
                TerraformSite {
                    faction_id,
                    target_z,
                },
                Transform::from_xyz(twp.x, twp.y, 0.3),
                GlobalTransform::default(),
                Visibility::Visible,
                InheritedVisibility::default(),
            ))
            .id();
        terraform_map.0.insert(tile, e);
    }
    site
}

/// Converts a freshly-placed `BuildSiteKind::Well` blueprint — from a manual
/// `Build Well`, a chief/organic `WaterAccess` intent, or any other producer —
/// into a staged [`WellSite`]. A well is a multi-phase excavation, not a
/// one-shot `construction_system` finalize, so the placeholder blueprint is
/// despawned and replaced. Runs in Sequential before `construction_system`, so
/// the blueprint never accumulates build progress. `TooDeep` sites are
/// rejected (blueprint despawned); `Unresolvable` ones are left for a later
/// tick (chunk still streaming in).
pub fn convert_well_blueprint_system(
    mut commands: Commands,
    globe: Res<Globe>,
    chunk_map: Res<ChunkMap>,
    mut bp_map: ResMut<BlueprintMap>,
    mut well_site_map: ResMut<WellSiteMap>,
    mut terraform_map: ResMut<TerraformMap>,
    structure_index: Res<StructureIndex>,
    well_map: Res<WellMap>,
    blueprints: Query<(Entity, &Blueprint)>,
) {
    for (entity, bp) in blueprints.iter() {
        if bp.kind != BuildSiteKind::Well {
            continue;
        }
        let center = bp.tile;
        // A WellSite already owns this tile — drop the stray blueprint.
        if well_site_map.0.contains_key(&center) {
            if bp_map.0.get(&center) == Some(&entity) {
                bp_map.0.remove(&center);
            }
            commands.entity(entity).despawn();
            continue;
        }
        match well_spec_at(&globe, &chunk_map, center) {
            WellResult::Unresolvable => continue, // chunk still loading — retry
            WellResult::TooDeep => {
                // Water table beyond a hand-dug shaft — reject the site.
                if bp_map.0.get(&center) == Some(&entity) {
                    bp_map.0.remove(&center);
                }
                commands.entity(entity).despawn();
            }
            WellResult::Ok(spec) => {
                // Footprint validation — the 5×5 stepwell must not overlap an
                // existing structure, blueprint, or another well. Manual and
                // organic placement only check the centre tile, so a well sited
                // one or two tiles from a hut would carve its helix through the
                // wall. Reject (despawn the blueprint) exactly like `TooDeep`.
                let footprint_clear = well_footprint(center).into_iter().all(|t| {
                    !structure_index.0.contains_key(&t)
                        && !well_map.0.contains_key(&t)
                        && !well_site_map.0.contains_key(&t)
                        && bp_map.0.get(&t).map_or(true, |&e| e == entity)
                });
                if !footprint_clear {
                    if bp_map.0.get(&center) == Some(&entity) {
                        bp_map.0.remove(&center);
                    }
                    commands.entity(entity).despawn();
                    continue;
                }
                let author = bp
                    .posted_by
                    .map(|e| BlueprintAuthor::new(e, bp.design_techs));
                spawn_well_site(
                    &mut commands,
                    &mut well_site_map,
                    &mut terraform_map,
                    bp.faction_id,
                    center,
                    spec,
                    author,
                );
                if bp_map.0.get(&center) == Some(&entity) {
                    bp_map.0.remove(&center);
                }
                commands.entity(entity).despawn();
            }
        }
    }
}

/// Drives every `WellSite` through Excavating → Lining → Capping. The single
/// staging authority. Sequential, after `construction_system` (so a lining
/// wall finalized this tick is already out of `BlueprintMap`).
pub fn well_site_progression_system(
    mut commands: Commands,
    terraform_map: Res<TerraformMap>,
    mut bp_map: ResMut<BlueprintMap>,
    mut well_map: ResMut<WellMap>,
    mut well_site_map: ResMut<WellSiteMap>,
    mut runtime_water: ResMut<RuntimeWater>,
    faction_registry: Res<FactionRegistry>,
    mut sites: Query<(Entity, &mut WellSite)>,
) {
    for (entity, mut site) in sites.iter_mut() {
        match site.phase {
            WellPhase::Excavating => {
                // Spawn the rim lining walls up front (first progression tick).
                // The `Blueprint`s give the chief a `JobKind::Build` to post,
                // which is what puts workers on `AgentGoal::Build` — and
                // `terraform_dispatch_system` routes idle Build-goal workers to
                // the excavation `TerraformSite`s. Without a live blueprint a
                // standalone well would never attract a digger. Walls and
                // shaft then progress in parallel (you can build the rim while
                // the hole is dug).
                if !site.walls_spawned {
                    let techs: FactionTechs = site
                        .author
                        .map(|a| a.design_techs)
                        .or_else(|| {
                            faction_registry
                                .factions
                                .get(&site.faction_id)
                                .map(|f| f.buildable_techs)
                        })
                        .unwrap_or_default();
                    let mat = best_wall_material(&techs);
                    let gateway = gateway_tile(site.center);
                    let mut lining = Vec::new();
                    for tile in outer_ring(site.center) {
                        if tile == gateway || bp_map.0.contains_key(&tile) {
                            continue;
                        }
                        let wp = tile_to_world(tile.0, tile.1);
                        let bp = Blueprint::new(
                            site.faction_id,
                            None,
                            BuildSiteKind::Wall(mat),
                            tile,
                            site.surf_z,
                        )
                        .with_author(site.author);
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
                        lining.push(tile);
                    }
                    site.lining_tiles = lining;
                    site.walls_spawned = true;
                }
                let spec = site.spec();
                let done = excavation_targets(&spec, site.center)
                    .iter()
                    .all(|(t, _)| !terraform_map.0.contains_key(t));
                if !done {
                    continue;
                }
                // Shaft is dug — charge the physical water column. The dig
                // broke into the saturated zone; the fluid sim recharges via
                // the seep source and caps it at the table.
                runtime_water.set(
                    site.center,
                    RuntimeWaterCell {
                        ground_z: site.bottom_z,
                        depth: WELL_INITIAL_CHARGE_Z,
                        reservoir_id: u32::MAX,
                        salinity: 0.0,
                        source_rate: AQUIFER_SEEP_RATE,
                    },
                );
                site.phase = WellPhase::Lining;
            }
            WellPhase::Lining => {
                let done = site.lining_tiles.iter().all(|t| !bp_map.0.contains_key(t));
                if done {
                    site.phase = WellPhase::Capping;
                }
            }
            WellPhase::Capping => {
                let wp = tile_to_world(site.center.0, site.center.1);
                let well = commands
                    .spawn((
                        Well {
                            faction_id: site.faction_id,
                            shaft_tile: site.center,
                            bottom_z: site.bottom_z,
                            surf_z: site.surf_z,
                        },
                        StructureLabel(BuildSiteKind::Well.label()),
                        Transform::from_xyz(wp.x, wp.y, 0.35),
                        GlobalTransform::default(),
                        Visibility::Visible,
                        InheritedVisibility::default(),
                    ))
                    .id();
                well_map.0.insert(site.center, well);
                well_site_map.0.remove(&site.center);
                commands.entity(entity).despawn();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Carved-geometry persistence
// ---------------------------------------------------------------------------

/// Re-carve one footprint tile down to `target_z`, one Z per `carve_tile`
/// call — exactly mirroring `terraform_system`'s stepwise descent. Idempotent:
/// a tile already at (or below) target is a no-op, and an unloaded chunk
/// reads `surface_z_at < Z_MIN` so the loop never runs. The guard bounds the
/// worst case to the hand-dug depth cap plus headroom.
fn recarve_tile_to(
    chunk_map: &mut ChunkMap,
    gen: &WorldGen,
    globe: &Globe,
    tile: (i32, i32),
    target_z: i8,
    tile_changed: &mut EventWriter<TileChangedEvent>,
) {
    let mut guard = 0;
    while chunk_map.surface_z_at(tile.0, tile.1) > target_z as i32 {
        let surf = chunk_map.surface_z_at(tile.0, tile.1);
        carve_tile(
            chunk_map,
            gen,
            globe,
            tile.0,
            tile.1,
            surf - 1,
            tile_changed,
        );
        guard += 1;
        if guard > MAX_HAND_DUG_WELL_DEPTH_Z + 4 {
            break;
        }
    }
}

/// Re-open a well's stepwell geometry on the live `ChunkMap`: the centre
/// water-shaft column and the descending helix of carved floors. Shared by
/// the seed-time carve pass and the chunk-reload restamp.
pub fn carve_well_geometry(
    chunk_map: &mut ChunkMap,
    gen: &WorldGen,
    globe: &Globe,
    center: (i32, i32),
    spec: WellSpec,
    tile_changed: &mut EventWriter<TileChangedEvent>,
) {
    for (tile, target_z) in excavation_targets(&spec, center) {
        recarve_tile_to(chunk_map, gen, globe, tile, target_z, tile_changed);
    }
}

/// OnEnter(Playing), after `seed_starting_buildings_system`. Seed wells are
/// stamped as finished `Well` entities (skipping the worker excavation
/// pipeline), so their visible stepwell shaft was never dug. Carve it now and
/// project the charged `RuntimeWater` column onto the freshly-opened centre
/// tile, so a seeded well looks dug — and reads as drinkable blue water —
/// from tick 0 rather than appearing as a flat tile.
pub fn carve_seeded_wells_system(
    mut chunk_map: ResMut<ChunkMap>,
    gen: Res<WorldGen>,
    globe: Res<Globe>,
    runtime_water: Res<RuntimeWater>,
    wells: Query<&Well>,
    mut tile_changed: EventWriter<TileChangedEvent>,
) {
    for well in wells.iter() {
        carve_well_geometry(
            &mut chunk_map,
            &gen,
            &globe,
            well.shaft_tile,
            well_spec_of(well),
            &mut tile_changed,
        );
        if let Some(cell) = runtime_water.cells.get(&well.shaft_tile) {
            if chunk_map.apply_water_column(
                well.shaft_tile.0,
                well.shaft_tile.1,
                cell.ground_z,
                cell.depth,
                cell.reservoir_id,
            ) {
                tile_changed.send(TileChangedEvent {
                    tx: well.shaft_tile.0,
                    ty: well.shaft_tile.1,
                });
            }
        }
    }
}

/// FixedUpdate, after `chunk_streaming_system`, before
/// `restamp_runtime_water_on_chunk_load`. Carved Air/floor tiles are chunk
/// deltas not re-applied on regen — only `WallMap`/`DamMap` entities and
/// `RuntimeWater` columns survive. A well whose 5×5 footprint straddles a
/// chunk boundary would lose the navigable helix on the off-chunk portion
/// when that chunk unloads and streams back. `Well` (`surf_z` + `bottom_z`)
/// and an in-progress `WellSite` are the durable truth: on every
/// `ChunkLoadedEvent` re-carve any well geometry intersecting a loaded chunk.
/// The lining `Wall` entities survive streaming on their own; the water
/// column is re-applied by `restamp_runtime_water_on_chunk_load` (chained
/// after this so it stamps water onto the just-re-carved shaft).
pub fn restamp_wells_on_chunk_load(
    mut events: EventReader<ChunkLoadedEvent>,
    mut chunk_map: ResMut<ChunkMap>,
    gen: Res<WorldGen>,
    globe: Res<Globe>,
    terraform_map: Res<TerraformMap>,
    wells: Query<&Well>,
    well_sites: Query<&WellSite>,
    mut tile_changed: EventWriter<TileChangedEvent>,
) {
    let loaded: AHashSet<ChunkCoord> = events.read().map(|e| e.coord).collect();
    if loaded.is_empty() {
        return;
    }
    let in_loaded = |tile: (i32, i32)| {
        loaded.contains(&ChunkCoord(
            tile.0.div_euclid(CHUNK_SIZE as i32),
            tile.1.div_euclid(CHUNK_SIZE as i32),
        ))
    };

    // Finished wells — re-carve the whole footprint if any tile reloaded.
    for well in wells.iter() {
        let spec = well_spec_of(well);
        let targets = excavation_targets(&spec, well.shaft_tile);
        if !targets.iter().any(|(t, _)| in_loaded(*t)) {
            continue;
        }
        for (tile, target_z) in targets {
            recarve_tile_to(
                &mut chunk_map,
                &gen,
                &globe,
                tile,
                target_z,
                &mut tile_changed,
            );
        }
    }

    // In-progress sites — re-carve only excavation tiles that have already
    // completed (drained from `TerraformMap`). A tile a worker is still
    // digging keeps its live `surface_z` progress and must not be jumped
    // ahead to its final target.
    for site in well_sites.iter() {
        let spec = site.spec();
        for (tile, target_z) in excavation_targets(&spec, site.center) {
            if !in_loaded(tile) || terraform_map.0.contains_key(&tile) {
                continue;
            }
            recarve_tile_to(
                &mut chunk_map,
                &gen,
                &globe,
                tile,
                target_z,
                &mut tile_changed,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn depth_derivation_clamps_and_rejects() {
        // Table 3 Z below the surface → needed = 3 + sump 1 = 4.
        match well_spec_from(10, 7.0) {
            WellResult::Ok(s) => {
                assert_eq!(s.depth, 4);
                assert_eq!(s.bottom_z, 6);
                assert_eq!(s.surf_z, 10);
            }
            other => panic!("expected Ok, got {other:?}"),
        }
        // Very wet: table at the surface → clamps up to MIN.
        match well_spec_from(10, 10.0) {
            WellResult::Ok(s) => assert_eq!(s.depth, MIN_WELL_DEPTH_Z),
            other => panic!("expected Ok, got {other:?}"),
        }
        // Arid: table far below → TooDeep.
        assert_eq!(well_spec_from(10, -20.0), WellResult::TooDeep);
        // Exactly at the cap is buildable.
        assert!(matches!(
            well_spec_from(10, (10 - MAX_HAND_DUG_WELL_DEPTH_Z + WELL_SUMP_Z) as f32),
            WellResult::Ok(_)
        ));
    }

    #[test]
    fn well_footprint_is_the_full_5x5() {
        let fp = well_footprint((3, 7));
        assert_eq!(fp.len(), 25);
        // Centre, every inner-ring tile, every outer-ring tile are members.
        assert!(fp.contains(&(3, 7)));
        for t in inner_ring((3, 7)) {
            assert!(fp.contains(&t), "inner-ring tile {t:?} missing");
        }
        for t in outer_ring((3, 7)) {
            assert!(fp.contains(&t), "outer-ring tile {t:?} missing");
        }
    }

    #[test]
    fn well_spec_of_round_trips_geometry() {
        let well = Well {
            faction_id: 1,
            shaft_tile: (0, 0),
            bottom_z: 4,
            surf_z: 10,
        };
        let spec = well_spec_of(&well);
        assert_eq!(spec.surf_z, 10);
        assert_eq!(spec.bottom_z, 4);
        assert_eq!(spec.depth, 6);
        // Helix length matches the depth — same invariant `well_spec_from` holds.
        assert_eq!(excavation_targets(&spec, (0, 0)).len(), 1 + 6);
    }

    #[test]
    fn inner_ring_is_a_walkable_chebyshev_loop() {
        let ring = inner_ring((0, 0));
        for k in 0..8 {
            let a = ring[k];
            let b = ring[(k + 1) % 8];
            let cheb = (a.0 - b.0).abs().max((a.1 - b.1).abs());
            assert_eq!(cheb, 1, "ring tiles {k} and {} must be adjacent", k + 1);
        }
    }

    #[test]
    fn outer_ring_has_16_tiles_and_a_gateway() {
        let outer = outer_ring((0, 0));
        assert_eq!(outer.len(), 16);
        assert!(outer.contains(&gateway_tile((0, 0))));
    }

    #[test]
    fn well_site_progresses_excavate_lining_capping() {
        use crate::simulation::construction::{BlueprintMap, WellMap};
        let mut app = App::new();
        app.insert_resource(TerraformMap::default());
        app.insert_resource(BlueprintMap::default());
        app.insert_resource(WellMap::default());
        app.insert_resource(WellSiteMap::default());
        app.insert_resource(RuntimeWater::default());
        app.insert_resource(FactionRegistry::default());
        app.add_systems(Update, well_site_progression_system);

        let center = (0, 0);
        let spec = WellSpec {
            surf_z: 5,
            bottom_z: 2,
            depth: 3,
        };
        // No TerraformSites registered → excavation reads complete.
        let site = app
            .world_mut()
            .spawn(WellSite::new(1, center, spec, None))
            .id();
        app.world_mut()
            .resource_mut::<WellSiteMap>()
            .0
            .insert(center, site);

        // Tick 1: Excavating → Lining (water charged, walls spawned).
        app.update();
        assert!(
            app.world()
                .resource::<RuntimeWater>()
                .cells
                .contains_key(&center),
            "shaft water column charged on excavation completion"
        );
        let lining = app
            .world()
            .get::<WellSite>(site)
            .unwrap()
            .lining_tiles
            .len();
        assert!(lining > 0, "lining walls spawned");
        assert_eq!(
            app.world().get::<WellSite>(site).unwrap().phase,
            WellPhase::Lining
        );

        // Simulate the lining walls being built (drained from BlueprintMap).
        app.world_mut().resource_mut::<BlueprintMap>().0.clear();
        // Tick 2: Lining → Capping. Tick 3: Capping → finished Well.
        app.update();
        app.update();
        assert!(
            app.world().resource::<WellMap>().0.contains_key(&center),
            "finished Well registered after capping"
        );
        assert!(!app
            .world()
            .resource::<WellSiteMap>()
            .0
            .contains_key(&center));
    }

    #[test]
    fn excavation_targets_descend_the_helix() {
        let spec = WellSpec {
            surf_z: 10,
            bottom_z: 4,
            depth: 6,
        };
        let targets = excavation_targets(&spec, (0, 0));
        // centre + 6 ring tiles
        assert_eq!(targets.len(), 7);
        assert_eq!(targets[0], ((0, 0), 4)); // centre → bottom_z
                                             // ring tile k → surf_z - k
        for k in 1..=6i32 {
            assert_eq!(targets[k as usize].1, (10 - k) as i8);
        }
    }
}
