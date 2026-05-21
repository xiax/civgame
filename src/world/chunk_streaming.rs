use ahash::{AHashMap, AHashSet};
use bevy::ecs::system::SystemParam;
use bevy::prelude::*;
use std::time::Instant;

use crate::economy::item::Item;
use crate::pathfinding::path_request::PathFollow;
use crate::rendering::camera::CameraViewZ;
use crate::rendering::color_map::{shaded_ore_tile_color, shaded_tile_color, z_bucket};
use crate::rendering::fog::{apply_fog_to_material, FogMap, FogTileMaterials};
use crate::rendering::projection::{
    skirt_sprite, ElevationSkirt, MapProjection, MapViewMode, ProjectedAnchor, ProjectionState,
};
use crate::simulation::construction::{DamMap, StructureLabel, Wall, WallMap, WallMaterial};
use crate::simulation::faction::{FactionCenter, StorageTileMap};
use crate::simulation::items::GroundItem;
use crate::simulation::plants::{
    spawn_plant_at, GrowthStage, PlantKind, PlantMap, PlantSpriteIndex,
};
use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE, Z_MIN};
use crate::world::globe::{Globe, GLOBE_CELL_CHUNKS, GLOBE_HEIGHT, GLOBE_WIDTH};
use crate::world::terrain::{
    generate_chunk_from_globe, tile_at_3d, tile_to_world, WorldGen, TILE_SIZE,
};
use crate::world::tile::{OreKind, TileKind};
use crate::world::water_runtime::RuntimeWater;

pub const LOAD_RADIUS: i32 = 12;
pub const UNLOAD_RADIUS: i32 = 16;

/// Smaller load radius for off-camera settled regions (data + sim only,
/// no sprites). Each region keeps a column of chunks active around its
/// centre so its agents can sim normally without being on screen.
pub const REGION_LOAD_RADIUS: i32 = 6;

/// Chunks that must NOT be unloaded by `chunk_streaming_system` even when
/// they're outside `UNLOAD_RADIUS` from the camera. Recomputed each tick by
/// `update_chunk_retention_system` from three sources:
///   - every `FactionCenter` entity's chunk (so agents can always path home)
///   - every storage tile's chunk (so `DepositResource` targets stay reachable)
///   - every chunk in any active agent's `PathFollow.chunk_route` (so a path
///     home doesn't go stale mid-traversal when the camera pans away)
///
/// Without this, `ChunkGraph` and `ChunkConnectivity` drop home chunks the
/// moment the camera follows a wandering agent past `UNLOAD_RADIUS`, which
/// the worker reports as `Unreachable` before A* even runs.
#[derive(Resource, Default)]
pub struct ChunkRetention {
    pub pinned: AHashSet<ChunkCoord>,
}

/// Emitted when `chunk_streaming_system` first inserts a chunk into
/// `ChunkMap.0`. Pathfinding listens for these to rebuild graph edges and
/// connectivity for the newly-loaded region.
#[derive(Event)]
pub struct ChunkLoadedEvent {
    pub coord: ChunkCoord,
}

/// Emitted when `chunk_streaming_system` removes a chunk from `ChunkMap.0`.
/// Pathfinding listens for these to drop stale graph edges.
#[derive(Event)]
pub struct ChunkUnloadedEvent {
    pub coord: ChunkCoord,
}

/// Emitted by `simulation::dig::dig_system` when an agent actually carves a
/// floor below the prior surface. Distinct from `TileChangedEvent` (which
/// fires for any tile mutation — wall stamping, road carving, plant
/// lifecycle, etc.) so consumers that only care about excavation —
/// specifically `water_runtime::aquifer_seep_emitter_system` — can react to
/// real digs without false-positives on every chunk-load wall stamp.
#[derive(Event)]
pub struct TileCarvedEvent {
    pub tx: i32,
    pub ty: i32,
    /// The new floor Z after the dig (`= surf_z - 1` in `dig_system`).
    pub new_floor_z: i32,
}

/// Bundles the load/unload event writers so `chunk_streaming_system` stays
/// under Bevy's 16-parameter system limit.
#[derive(SystemParam)]
pub struct ChunkStreamEvents<'w> {
    pub loaded: EventWriter<'w, ChunkLoadedEvent>,
    pub unloaded: EventWriter<'w, ChunkUnloadedEvent>,
}

/// Sub-bundle pulling sim-focus + map-view-projection into one slot of
/// `chunk_streaming_system`'s param list. Keeps the system under Bevy's
/// 16-param ceiling.
#[derive(SystemParam)]
pub struct StreamFocusParams<'w> {
    pub focus: Res<'w, crate::simulation::region::SimulationFocus>,
    pub view_projection: crate::rendering::projection::ViewProjection<'w>,
}

/// Bundle of plant-spawn state for `chunk_streaming_system`. Holds the live
/// `PlantMap`/`PlantSpriteIndex` mutably plus the read-only `SeedReservation`
/// so the per-chunk plant seeder can skip tiles reserved by the bootstrap
/// pipeline.
#[derive(SystemParam)]
pub struct StreamPlantParams<'w> {
    pub plant_map: ResMut<'w, PlantMap>,
    pub plant_sprite_index: ResMut<'w, PlantSpriteIndex>,
    pub seed_reservation: Res<'w, crate::simulation::seed_reservation::SeedReservation>,
}

/// Rebuild `SimulationFocus` from camera + every settled region's mega-chunk
/// centre. Runs each tick before `chunk_streaming_system` so the loader sees
/// the current focus set.
pub fn update_simulation_focus_system(
    mut focus: ResMut<crate::simulation::region::SimulationFocus>,
    settled: Res<crate::simulation::region::SettledRegions>,
    camera_q: Query<&Transform, With<Camera>>,
    map_view_mode: Res<crate::rendering::projection::MapViewMode>,
    map_projection: Res<crate::rendering::projection::MapProjection>,
) {
    use crate::simulation::region::{FocusPoint, MegaChunkCoord};

    focus.points.clear();

    if let Ok(cam) = camera_q.get_single() {
        // Convert camera position from view-space (potentially tilted) to
        // logical world coords so chunk loading tracks the tile actually
        // centred on screen, not the projected pixel position.
        let logical = crate::rendering::projection::camera_view_to_logical(
            cam.translation.truncate(),
            *map_view_mode,
            &map_projection,
        );
        focus.points.push(FocusPoint {
            world_pos: logical,
            chunk_radius: LOAD_RADIUS,
            is_camera: true,
        });
    }

    for region in settled.by_id.values() {
        let (tx, ty) = MegaChunkCoord::center_tile(region.megachunk.0, region.megachunk.1);
        let world_pos = Vec2::new(
            tx as f32 * TILE_SIZE + TILE_SIZE * 0.5,
            ty as f32 * TILE_SIZE + TILE_SIZE * 0.5,
        );
        focus.points.push(FocusPoint {
            world_pos,
            chunk_radius: REGION_LOAD_RADIUS,
            is_camera: false,
        });
    }
}

/// Runs each tick before `chunk_streaming_system`. Rebuilds `ChunkRetention`
/// from FactionCenter / StorageTileMap / PathFollow, plus every player-/AI-
/// affected water tile (dams + persisted runtime/seep cells). Cheap —
/// bounded by (factions + storage tiles + active path lengths + dams + seep
/// cells), well under a millisecond in practice.
///
/// **Water retention is the v2 "persistence" mechanism.** `RuntimeWater` is
/// off-chunk and survives unload *within a session*, but this engine has no
/// ECS save/load — only `world.bin` (the Globe) serialises; everything else
/// regenerates live from `Globe + seed`, so cross-process persistence of
/// runtime water is N/A by design. Pinning the chunks under a dam pool or a
/// dug-aquifer seep keeps the fluid sim's region + its backing tiles
/// resident as the player roams, so pan-away/back stays desync-free without
/// leaning solely on the reload restamp.
pub fn update_chunk_retention_system(
    mut retention: ResMut<ChunkRetention>,
    storage: Res<StorageTileMap>,
    dams: Res<DamMap>,
    runtime_water: Res<RuntimeWater>,
    centers: Query<&Transform, With<FactionCenter>>,
    follows: Query<&PathFollow>,
) {
    retention.pinned.clear();

    let chunk_of = |tx: i32, ty: i32| {
        ChunkCoord(
            tx.div_euclid(CHUNK_SIZE as i32),
            ty.div_euclid(CHUNK_SIZE as i32),
        )
    };

    for transform in &centers {
        let coord = chunk_coord_from_world(transform.translation.x, transform.translation.y);
        retention.pinned.insert(coord);
    }

    for &(tx, ty) in storage.tiles.keys() {
        retention.pinned.insert(chunk_of(tx, ty));
    }

    for follow in &follows {
        for &coord in &follow.chunk_route {
            retention.pinned.insert(coord);
        }
    }

    // Dams (durable truth) + every standing or spring-fed runtime cell.
    for &(tx, ty) in dams.0.keys() {
        retention.pinned.insert(chunk_of(tx, ty));
    }
    for (&(tx, ty), cell) in runtime_water.cells.iter() {
        if cell.depth > 0.0 || cell.source_rate > 0.0 {
            retention.pinned.insert(chunk_of(tx, ty));
        }
    }
}

fn chunk_coord_from_world(x: f32, y: f32) -> ChunkCoord {
    let tx = (x / TILE_SIZE).floor() as i32;
    let ty = (y / TILE_SIZE).floor() as i32;
    ChunkCoord(
        tx.div_euclid(CHUNK_SIZE as i32),
        ty.div_euclid(CHUNK_SIZE as i32),
    )
}

const PLANT_HASH_SEED: u32 = 42;

/// Coarse-cell size (in tiles) for resource patch masking. A `patch_hash` lookup
/// at this granularity decides whether a tile is inside a patch; when it is, the
/// per-tile hash gates spawn density at a much higher rate. The result: discrete
/// groves / berry patches / rock fields rather than a uniform carpet.
const PATCH_CELL_SIZE: i32 = 6;
const ROCK_PATCH_SEED: u32 = 0xCAFE_F00D;
const TREE_PATCH_SEED: u32 = 0xB0A7_C0DE;
const BERRY_PATCH_SEED: u32 = 0x5EED_B41D;

fn patch_hash(gx: i32, gy: i32, cell: i32, seed: u32) -> u32 {
    let cx = gx.div_euclid(cell);
    let cy = gy.div_euclid(cell);
    (cx.wrapping_mul(2_654_435_761_u32 as i32)
        ^ cy.wrapping_mul(2_246_822_519_u32 as i32)
        ^ seed as i32) as u32
}

/// One ColorMaterial per (TileKind, OreKind, z_bucket) tuple.
/// `OreKind` is `None` (0) for non-ore tiles. `TileKind::Ore` fans out into one
/// material per non-None OreKind so per-ore colors render distinctly.
#[derive(Resource, Default)]
pub struct TileMaterials {
    pub materials: AHashMap<(u8, u8, i32), Handle<ColorMaterial>>,
    pub tile_mesh: Handle<Mesh>,
}

impl TileMaterials {
    pub fn handle_for(&self, kind: TileKind, ore: OreKind, z: i32) -> Handle<ColorMaterial> {
        self.materials
            .get(&(kind as u8, ore as u8, z_bucket(z)))
            .cloned()
            .unwrap_or_default()
    }
}

#[derive(Resource)]
pub struct ChunkBoundaryOverlay {
    pub show: bool,
}

impl Default for ChunkBoundaryOverlay {
    fn default() -> Self {
        Self { show: false }
    }
}

pub fn toggle_chunk_boundary_overlay_system(
    keys: Res<ButtonInput<KeyCode>>,
    mut overlay: ResMut<ChunkBoundaryOverlay>,
) {
    if keys.just_pressed(KeyCode::F3) {
        overlay.show = !overlay.show;
    }
}

pub fn chunk_boundary_gizmo_system(
    overlay: Res<ChunkBoundaryOverlay>,
    mut gizmos: Gizmos,
    camera_query: Query<(&Transform, &OrthographicProjection), With<Camera>>,
    windows: Query<&Window>,
) {
    if !overlay.show {
        return;
    }
    let Ok((transform, projection)) = camera_query.get_single() else {
        return;
    };
    let Ok(window) = windows.get_single() else {
        return;
    };

    let chunk_world = CHUNK_SIZE as f32 * TILE_SIZE;
    let half_w = window.width() * 0.5 * projection.scale;
    let half_h = window.height() * 0.5 * projection.scale;
    let cam = transform.translation.truncate();
    let x_min = cam.x - half_w;
    let x_max = cam.x + half_w;
    let y_min = cam.y - half_h;
    let y_max = cam.y + half_h;

    let cx_min = (x_min / chunk_world).floor() as i32;
    let cx_max = (x_max / chunk_world).ceil() as i32;
    let cy_min = (y_min / chunk_world).floor() as i32;
    let cy_max = (y_max / chunk_world).ceil() as i32;

    let color = Color::srgba(1.0, 0.85, 0.2, 0.55);

    for cx in cx_min..=cx_max {
        let x = cx as f32 * chunk_world;
        gizmos.line_2d(Vec2::new(x, y_min), Vec2::new(x, y_max), color);
    }
    for cy in cy_min..=cy_max {
        let y = cy as f32 * chunk_world;
        gizmos.line_2d(Vec2::new(x_min, y), Vec2::new(x_max, y), color);
    }
}

#[derive(Resource, Default)]
pub struct TileSpriteIndex {
    pub by_chunk: AHashMap<ChunkCoord, Vec<Entity>>,
    /// Per-tile lookup for TileSprite entities (excludes Wall entities).
    pub by_tile: AHashMap<(i32, i32), Entity>,
    /// Per-tile lookup for `ElevationSkirt` entities (south face only).
    /// Used by `attach_late_south_skirts_system` to detect tiles that
    /// missed their skirt at chunk-spawn time because the southern
    /// neighbour's chunk hadn't loaded yet.
    pub skirt_by_tile: AHashMap<(i32, i32), Entity>,
}

#[derive(Component)]
pub struct TileSprite;

/// Fired by dig_system when a tile's surface changes. The rendering layer
/// despawns the old sprite and spawns a new one matching the updated terrain.
#[derive(Event)]
pub struct TileChangedEvent {
    pub tx: i32,
    pub ty: i32,
}

pub const RENDERABLE_KINDS: &[TileKind] = &[
    TileKind::Grass,
    TileKind::Water,
    TileKind::River,
    TileKind::Stone,
    TileKind::Forest,
    TileKind::Sand,
    TileKind::Road,
    TileKind::Wall,
    TileKind::Ramp,
    TileKind::Dirt,
    TileKind::Ore,
    // New surfaces
    TileKind::Snow,
    TileKind::Marsh,
    TileKind::Scrub,
    // Stone variants
    TileKind::Granite,
    TileKind::Limestone,
    TileKind::Sandstone,
    TileKind::Basalt,
    // Soil variants
    TileKind::Loam,
    TileKind::Silt,
    TileKind::Clay,
    TileKind::SandySoil,
    TileKind::Cropland,
];

/// Ore variants rendered as `TileKind::Ore` tiles. Excludes `OreKind::None`,
/// which never reaches the renderer.
pub const RENDERABLE_ORES: &[OreKind] = &[
    OreKind::Copper,
    OreKind::Tin,
    OreKind::Iron,
    OreKind::Coal,
    OreKind::Gold,
    OreKind::Silver,
];

/// PostStartup: create one shaded ColorMaterial per (TileKind, z_bucket) pair.
pub fn setup_tile_materials(
    mut tile_materials: ResMut<TileMaterials>,
    mut materials: ResMut<Assets<ColorMaterial>>,
    mut meshes: ResMut<Assets<Mesh>>,
) {
    tile_materials.tile_mesh = meshes.add(Rectangle::new(TILE_SIZE - 0.5, TILE_SIZE - 0.5));

    let bucket_min = Z_MIN.div_euclid(4);
    let bucket_max = 15_i32.div_euclid(4);

    for &kind in RENDERABLE_KINDS {
        if kind == TileKind::Ore {
            for &ore in RENDERABLE_ORES {
                for bucket in bucket_min..=bucket_max {
                    let z = bucket * 4 + 2;
                    let color = shaded_ore_tile_color(ore, z);
                    let handle = materials.add(ColorMaterial::from_color(color));
                    tile_materials
                        .materials
                        .insert((kind as u8, ore as u8, bucket), handle);
                }
            }
            continue;
        }
        for bucket in bucket_min..=bucket_max {
            let z = bucket * 4 + 2;
            let color = shaded_tile_color(kind, z);
            let handle = materials.add(ColorMaterial::from_color(color));
            tile_materials
                .materials
                .insert((kind as u8, OreKind::None as u8, bucket), handle);
        }
    }
}

/// Spawn tile sprites for a single chunk; populates both by_chunk and by_tile.
#[allow(clippy::too_many_arguments)]
pub fn spawn_chunk_sprites(
    commands: &mut Commands,
    tile_materials: &TileMaterials,
    fog_tile_materials: &FogTileMaterials,
    fog_map: &FogMap,
    sprite_index: &mut TileSpriteIndex,
    wall_map: &mut WallMap,
    chunk_map: &ChunkMap,
    gen: &WorldGen,
    globe: &Globe,
    coord: ChunkCoord,
    camera_view_z: i32,
    map_view_mode: MapViewMode,
    map_projection: &MapProjection,
) {
    let Some(chunk) = chunk_map.0.get(&coord) else {
        return;
    };
    if sprite_index.by_chunk.contains_key(&coord) {
        return;
    }

    let mut entities = Vec::with_capacity(CHUNK_SIZE * CHUNK_SIZE);

    for ty in 0..CHUNK_SIZE {
        for tx in 0..CHUNK_SIZE {
            let global_tx = coord.0 * CHUNK_SIZE as i32 + tx as i32;
            let global_ty = coord.1 * CHUNK_SIZE as i32 + ty as i32;
            let surf_z = chunk.surface_z[ty][tx] as i32;
            let kind = chunk.surface_tile_kind(tx, ty);

            if kind == TileKind::Air {
                continue;
            }

            let wx = global_tx as f32 * TILE_SIZE + TILE_SIZE * 0.5;
            let wy = global_ty as f32 * TILE_SIZE + TILE_SIZE * 0.5;
            let tile_pos = (global_tx as i32, global_ty as i32);

            if kind == TileKind::Wall {
                if !wall_map.0.contains_key(&tile_pos) {
                    let entity = commands
                        .spawn((
                            Wall {
                                material: WallMaterial::Stone,
                            },
                            StructureLabel(WallMaterial::Stone.label()),
                            Transform::from_xyz(wx, wy, 0.4),
                            GlobalTransform::default(),
                            Visibility::Visible,
                            InheritedVisibility::default(),
                            ProjectedAnchor::Static {
                                z: surf_z.clamp(i8::MIN as i32, i8::MAX as i32) as i8,
                            },
                            ProjectionState::default(),
                        ))
                        .id();
                    wall_map.0.insert(tile_pos, entity);
                    entities.push(entity);
                } else if let Some(&entity) = wall_map.0.get(&tile_pos) {
                    entities.push(entity);
                }
                continue;
            }

            // Compute the effective render Z and tile for this position
            let (render_kind, render_ore, render_z, base_vis) = resolve_render_tile(
                chunk_map,
                gen,
                globe,
                global_tx,
                global_ty,
                surf_z,
                camera_view_z,
            );

            let mut initial_mat =
                MeshMaterial2d(tile_materials.handle_for(render_kind, render_ore, render_z));
            let visibility = apply_fog_to_material(
                fog_map,
                tile_pos,
                base_vis,
                render_kind,
                render_ore,
                render_z,
                tile_materials,
                fog_tile_materials,
                &mut initial_mat,
            );

            let entity = commands
                .spawn((
                    TileSprite,
                    Mesh2d(tile_materials.tile_mesh.clone()),
                    initial_mat,
                    Transform::from_xyz(wx, wy, 0.0),
                    GlobalTransform::default(),
                    visibility,
                    InheritedVisibility::default(),
                    ProjectedAnchor::Static {
                        z: surf_z.clamp(i8::MIN as i32, i8::MAX as i32) as i8,
                    },
                    ProjectionState::default(),
                ))
                .id();
            entities.push(entity);
            sprite_index.by_tile.insert(tile_pos, entity);

            // South-facing cliff skirt — only attaches when this tile is
            // taller than its (already-loaded) southern neighbour. Neighbours
            // in unloaded chunks return `Z_MIN - 1` so they're skipped here;
            // `attach_late_south_skirts_system` back-fills them when the
            // southern chunk loads.
            let south_z = chunk_map.surface_z_at(global_tx, global_ty - 1);
            if south_z >= Z_MIN && surf_z > south_z {
                let delta = (surf_z - south_z) as u8;
                let skirt = spawn_skirt_for_tile(
                    commands,
                    &mut entities,
                    global_tx,
                    global_ty,
                    south_z,
                    delta,
                    map_view_mode,
                    map_projection,
                );
                sprite_index
                    .skirt_by_tile
                    .insert((global_tx, global_ty), skirt);
            }
        }
    }

    sprite_index.by_chunk.insert(coord, entities);
}

/// Spawn the south-facing cliff skirt sibling for a tile. Anchored at the
/// south neighbour's projected top edge so the visual band exactly fills
/// the elevation gap; world position uses the south neighbour's surface_z
/// for `ProjectedAnchor::Static` so it inherits that neighbour's lift.
/// Returns the spawned entity so the caller can register it in
/// `TileSpriteIndex.skirt_by_tile`.
#[allow(clippy::too_many_arguments)]
fn spawn_skirt_for_tile(
    commands: &mut Commands,
    entities: &mut Vec<Entity>,
    tx: i32,
    ty: i32,
    south_z: i32,
    delta_z: u8,
    map_view_mode: MapViewMode,
    map_projection: &MapProjection,
) -> Entity {
    // Logical anchor: midpoint of the southern shared edge between (tx, ty)
    // and (tx, ty - 1). In TopDown this collapses to a thin seam under the
    // tile; in Tilted the projection lift makes the skirt's top edge land
    // exactly on the upper tile's projected south edge.
    let wx = tx as f32 * TILE_SIZE + TILE_SIZE * 0.5;
    let wy = ty as f32 * TILE_SIZE; // shared edge logical y
    let z_layer = 0.05; // just above terrain (0.0), below entities (0.5)
    let entity = commands
        .spawn((
            ElevationSkirt { delta_z },
            skirt_sprite(delta_z, map_projection, map_view_mode),
            Transform::from_xyz(wx, wy, z_layer),
            GlobalTransform::default(),
            if map_view_mode == MapViewMode::Tilted {
                Visibility::Inherited
            } else {
                Visibility::Hidden
            },
            InheritedVisibility::default(),
            ProjectedAnchor::Static {
                z: south_z.clamp(i8::MIN as i32, i8::MAX as i32) as i8,
            },
            ProjectionState::default(),
        ))
        .id();
    entities.push(entity);
    entity
}

/// Back-fill south skirts that were skipped at chunk-spawn time because
/// the southern neighbour's chunk wasn't loaded yet. Fires on every
/// `ChunkLoadedEvent`: for the row of tiles immediately *north* of the
/// freshly-loaded chunk's top edge, those tiles' southern neighbours are
/// now loaded — so we can compute the elevation step and spawn a skirt
/// for any cliff that was missed.
///
/// Bounded to one row per loaded chunk per fire (32 tiles). Cheap.
pub fn attach_late_south_skirts_system(
    mut commands: Commands,
    mut events: EventReader<ChunkLoadedEvent>,
    chunk_map: Res<ChunkMap>,
    mut sprite_index: ResMut<TileSpriteIndex>,
    view_projection: crate::rendering::projection::ViewProjection,
) {
    let map_view_mode = *view_projection.mode;
    let map_projection = *view_projection.proj;

    for ev in events.read() {
        let coord = ev.coord;
        // Tiles immediately north of this chunk's top row would have been
        // spawned by an earlier load of the chunk at (coord.0, coord.1+1).
        // Their `ty - 1` lookups now hit the freshly-loaded chunk.
        let x0 = coord.0 * CHUNK_SIZE as i32;
        let north_ty = (coord.1 + 1) * CHUNK_SIZE as i32;
        for lx in 0..CHUNK_SIZE as i32 {
            let tx = x0 + lx;
            let ty = north_ty;
            // Only act if a TileSprite exists (i.e. the northern chunk is
            // loaded with sprites) AND no skirt has been registered yet.
            if !sprite_index.by_tile.contains_key(&(tx, ty))
                || sprite_index.skirt_by_tile.contains_key(&(tx, ty))
            {
                continue;
            }
            let our_z = chunk_map.surface_z_at(tx, ty);
            let south_z = chunk_map.surface_z_at(tx, ty - 1);
            if our_z < Z_MIN || south_z < Z_MIN || our_z <= south_z {
                continue;
            }
            let delta = (our_z - south_z) as u8;
            // Skirt belongs to the *northern* tile's chunk so it gets
            // cleaned up when that chunk unloads.
            let northern_chunk = ChunkCoord(coord.0, coord.1 + 1);
            let entities = sprite_index.by_chunk.entry(northern_chunk).or_default();
            let skirt = spawn_skirt_for_tile(
                &mut commands,
                entities,
                tx,
                ty,
                south_z,
                delta,
                map_view_mode,
                &map_projection,
            );
            sprite_index.skirt_by_tile.insert((tx, ty), skirt);
        }
    }
}

/// Determine what to render at a tile given the camera view Z.
/// Returns (kind, ore, z_for_shading, visibility). `ore` is `OreKind::None`
/// for non-ore tiles.
pub fn resolve_render_tile(
    chunk_map: &ChunkMap,
    gen: &WorldGen,
    globe: &Globe,
    tx: i32,
    ty: i32,
    surf_z: i32,
    camera_view_z: i32,
) -> (TileKind, OreKind, i32, Visibility) {
    if camera_view_z == i32::MAX || surf_z <= camera_view_z {
        // Surface tile is at or below the view level — render normally.
        // Surface tiles never carry ore (ore only exists subsurface), so
        // OreKind::None is correct here.
        let kind = chunk_map.tile_kind_at(tx, ty).unwrap_or(TileKind::Air);
        if kind == TileKind::Air {
            return (TileKind::Grass, OreKind::None, surf_z, Visibility::Hidden);
        }
        return (kind, OreKind::None, surf_z, Visibility::Visible);
    }
    // Surface tile is above the view level — show what's at camera_view_z instead
    let underground_tile = tile_at_3d(chunk_map, gen, globe, tx, ty, camera_view_z);
    if underground_tile.kind == TileKind::Air {
        (
            TileKind::Grass,
            OreKind::None,
            camera_view_z,
            Visibility::Hidden,
        )
    } else {
        (
            underground_tile.kind,
            underground_tile.ore_kind(),
            camera_view_z,
            Visibility::Visible,
        )
    }
}

/// Deterministically seed initial plants for a chunk. Skips any tile the
/// bootstrap pipeline reserved (footprint, doormat, planned road, ag plot)
/// — without this gate, a chunk streaming in after warmup would scatter
/// wild grain/berries onto seeded house roofs or planned roads.
pub fn spawn_chunk_plants(
    commands: &mut Commands,
    plant_map: &mut PlantMap,
    plant_sprite_index: &mut PlantSpriteIndex,
    chunk_map: &ChunkMap,
    gen: &WorldGen,
    globe: &Globe,
    reservation: &crate::simulation::seed_reservation::SeedReservation,
    coord: ChunkCoord,
) {
    let Some(chunk) = chunk_map.0.get(&coord) else {
        return;
    };

    for ty in 0..CHUNK_SIZE {
        for tx in 0..CHUNK_SIZE {
            let global_tx = coord.0 * CHUNK_SIZE as i32 + tx as i32;
            let global_ty = coord.1 * CHUNK_SIZE as i32 + ty as i32;
            if reservation.is_reserved((global_tx, global_ty)) {
                continue;
            }
            let surf_z = chunk.surface_z[ty][tx] as i32;
            let tile = tile_at_3d(chunk_map, gen, globe, global_tx, global_ty, surf_z);

            let h = (global_tx.wrapping_mul(2_654_435_761_u32 as i32)
                ^ global_ty.wrapping_mul(2_246_822_519_u32 as i32)
                ^ PLANT_HASH_SEED as i32) as u32;

            match tile.kind {
                // High-fertility grassland — replaces the old Farmland branch.
                // Wild Grain (wheat) and BerryBush spawn on the most fertile
                // patches of grass. Effectively the natural prairie / meadow.
                TileKind::Grass if tile.fertility > 180 => {
                    let pct = h % 100;
                    let (kind, stage) = if pct < 4 {
                        (PlantKind::BerryBush, initial_stage(h))
                    } else if pct < 12 {
                        (PlantKind::Grain, initial_stage(h))
                    } else {
                        continue;
                    };
                    spawn_plant_at(
                        commands,
                        plant_map,
                        plant_sprite_index,
                        global_tx,
                        global_ty,
                        kind,
                        stage,
                    );
                }
                // Wetland-edge berries (no grain — too wet for cereal).
                TileKind::Marsh => {
                    if h % 100 < 5 {
                        spawn_plant_at(
                            commands,
                            plant_map,
                            plant_sprite_index,
                            global_tx,
                            global_ty,
                            PlantKind::BerryBush,
                            initial_stage(h),
                        );
                    }
                }
                TileKind::Grass if tile.fertility > 100 => {
                    let pct = h % 100;
                    let berry_patch =
                        patch_hash(global_tx, global_ty, PATCH_CELL_SIZE, BERRY_PATCH_SEED) % 100
                            < 6;
                    let tree_patch =
                        patch_hash(global_tx, global_ty, PATCH_CELL_SIZE, TREE_PATCH_SEED) % 100
                            < 12;
                    if berry_patch && pct < 40 {
                        spawn_plant_at(
                            commands,
                            plant_map,
                            plant_sprite_index,
                            global_tx,
                            global_ty,
                            PlantKind::BerryBush,
                            initial_stage(h),
                        );
                    } else if tree_patch && pct < 50 {
                        spawn_plant_at(
                            commands,
                            plant_map,
                            plant_sprite_index,
                            global_tx,
                            global_ty,
                            PlantKind::Tree,
                            initial_stage(h),
                        );
                    }
                }
                TileKind::Forest => {
                    if h % 100 < 40 {
                        spawn_plant_at(
                            commands,
                            plant_map,
                            plant_sprite_index,
                            global_tx,
                            global_ty,
                            PlantKind::Tree,
                            initial_stage(h),
                        );
                    }
                }
                _ => {}
            }
        }
    }
}

fn initial_stage(h: u32) -> GrowthStage {
    match h % 2 {
        0 => GrowthStage::Seedling,
        _ => GrowthStage::Mature,
    }
}

const ROCK_HASH_SEED: u32 = 0xDEAD_C0DE;

/// Deterministically scatter loose stone items across Stone surface tiles in a chunk.
pub fn spawn_chunk_loose_rocks(commands: &mut Commands, chunk_map: &ChunkMap, coord: ChunkCoord) {
    let Some(chunk) = chunk_map.0.get(&coord) else {
        return;
    };

    for ty in 0..CHUNK_SIZE {
        for tx in 0..CHUNK_SIZE {
            if chunk.surface_tile_kind(tx, ty) != TileKind::Stone {
                continue;
            }
            let global_tx = coord.0 * CHUNK_SIZE as i32 + tx as i32;
            let global_ty = coord.1 * CHUNK_SIZE as i32 + ty as i32;

            if patch_hash(global_tx, global_ty, PATCH_CELL_SIZE, ROCK_PATCH_SEED) % 100 >= 30 {
                continue;
            }

            let h = (global_tx.wrapping_mul(2_654_435_761_u32 as i32)
                ^ global_ty.wrapping_mul(2_246_822_519_u32 as i32)
                ^ ROCK_HASH_SEED as i32) as u32;

            if h % 100 >= 70 {
                continue;
            }

            let qty = (h % 3 + 1) as u32;
            let world_pos = tile_to_world(global_tx, global_ty);
            let surf_z = chunk.surface_z[ty][tx] as i32;
            commands.spawn((
                GroundItem {
                    item: Item::new_commodity(crate::economy::core_ids::stone()),
                    qty,
                },
                Transform::from_xyz(world_pos.x, world_pos.y, 0.3),
                GlobalTransform::default(),
                Visibility::Visible,
                InheritedVisibility::default(),
                crate::world::spatial::Indexed::new(crate::world::spatial::IndexedKind::GroundItem),
                crate::simulation::obstacle::ConstructionObstacle {
                    resolution: crate::simulation::obstacle::ObstacleResolution::Relocate,
                },
                ProjectedAnchor::Static {
                    z: surf_z.clamp(i8::MIN as i32, i8::MAX as i32) as i8,
                },
                ProjectionState::default(),
            ));
        }
    }
}

/// Update: stream chunks in/out as the camera moves.
pub fn chunk_streaming_system(
    mut has_run: Local<bool>,
    mut commands: Commands,
    tile_materials: Res<TileMaterials>,
    fog_tile_materials: Res<FogTileMaterials>,
    fog_map: Res<FogMap>,
    mut sprite_index: ResMut<TileSpriteIndex>,
    mut wall_map: ResMut<WallMap>,
    mut chunk_map: ResMut<ChunkMap>,
    gen: Res<WorldGen>,
    mut globe: ResMut<Globe>,
    mut plant_params: StreamPlantParams,
    camera_view_z: Res<CameraViewZ>,
    retention: Res<ChunkRetention>,
    mut stream_events: ChunkStreamEvents,
    focus_view: StreamFocusParams,
) {
    let now = Instant::now();
    let focus = &focus_view.focus;
    if focus.points.is_empty() {
        return;
    }
    let map_view_mode = *focus_view.view_projection.mode;
    let map_projection = *focus_view.view_projection.proj;

    let total_cx = GLOBE_WIDTH * GLOBE_CELL_CHUNKS;
    let total_cy = GLOBE_HEIGHT * GLOBE_CELL_CHUNKS;

    // Compute each focus's chunk-coord centre once.
    let focus_centres: Vec<(i32, i32, i32, bool)> = focus
        .points
        .iter()
        .map(|p| {
            let pcx = (p.world_pos.x / (CHUNK_SIZE as f32 * TILE_SIZE)).floor() as i32;
            let pcy = (p.world_pos.y / (CHUNK_SIZE as f32 * TILE_SIZE)).floor() as i32;
            (pcx, pcy, p.chunk_radius, p.is_camera)
        })
        .collect();

    // --- Load chunks within union of focus discs ---
    // `seen` prevents duplicate work when discs overlap.
    let mut seen: AHashSet<ChunkCoord> = AHashSet::default();
    for &(pcx, pcy, radius, is_camera) in &focus_centres {
        for dy in -radius..=radius {
            for dx in -radius..=radius {
                let cx = pcx + dx;
                let cy = pcy + dy;
                if cx < 0 || cy < 0 || cx >= total_cx || cy >= total_cy {
                    continue;
                }
                let coord = ChunkCoord(cx, cy);
                if !seen.insert(coord) {
                    // Already processed by a prior focus; if either focus is
                    // the camera we still need to spawn sprites — so re-run
                    // sprite check below regardless.
                }
                let (gx, gy) = Globe::cell_for_chunk(cx, cy);

                // 1. Ensure the chunk data exists in ChunkMap.
                if !chunk_map.0.contains_key(&coord) {
                    if globe.cell(gx, gy).is_none() {
                        continue;
                    }
                    let chunk = generate_chunk_from_globe(coord, &globe, &gen);
                    chunk_map.0.insert(coord, chunk);
                    stream_events.loaded.send(ChunkLoadedEvent { coord });

                    if let Some(gc) = globe.cell_mut(gx, gy) {
                        gc.explored = true;
                    }
                }

                // 2. Sprites only spawn for the camera focus.
                if is_camera && !sprite_index.by_chunk.contains_key(&coord) {
                    spawn_chunk_sprites(
                        &mut commands,
                        &tile_materials,
                        &fog_tile_materials,
                        &fog_map,
                        &mut sprite_index,
                        &mut wall_map,
                        &chunk_map,
                        &gen,
                        &globe,
                        coord,
                        camera_view_z.0,
                        map_view_mode,
                        &map_projection,
                    );

                    spawn_chunk_plants(
                        &mut commands,
                        &mut plant_params.plant_map,
                        &mut plant_params.plant_sprite_index,
                        &chunk_map,
                        &gen,
                        &globe,
                        &plant_params.seed_reservation,
                        coord,
                    );

                    spawn_chunk_loose_rocks(&mut commands, &chunk_map, coord);
                }
            }
        }
    }

    // --- Unload chunks beyond UNLOAD_RADIUS of every focus ---
    // A chunk is kept if (a) pinned by ChunkRetention or (b) within
    // (focus.chunk_radius + unload_extra) of any focus point. unload_extra
    // matches the original UNLOAD_RADIUS - LOAD_RADIUS = 4 chunk margin.
    let unload_extra = UNLOAD_RADIUS - LOAD_RADIUS;
    let to_unload: Vec<ChunkCoord> = chunk_map
        .0
        .keys()
        .copied()
        .filter(|&c| {
            if retention.pinned.contains(&c) {
                return false;
            }
            for &(pcx, pcy, radius, _) in &focus_centres {
                let dx = (c.0 - pcx).abs();
                let dy = (c.1 - pcy).abs();
                if dx.max(dy) <= radius + unload_extra {
                    return false;
                }
            }
            true
        })
        .collect();

    for coord in to_unload {
        chunk_map.0.remove(&coord);
        stream_events.unloaded.send(ChunkUnloadedEvent { coord });

        let x0 = (coord.0 * CHUNK_SIZE as i32) as i32;
        let y0 = (coord.1 * CHUNK_SIZE as i32) as i32;

        // Optimization: iterate locally over chunk tiles instead of scanning the whole map.
        // Wall entities are durable structures — leave `wall_map` entries intact across
        // unload/reload so the streaming reload path reuses them instead of spawning a
        // generic Stone replacement that would lose the original material + StructureLabel.
        for ly in 0..CHUNK_SIZE as i32 {
            for lx in 0..CHUNK_SIZE as i32 {
                let tx = x0 + lx;
                let ty = y0 + ly;
                sprite_index.by_tile.remove(&(tx, ty));
                sprite_index.skirt_by_tile.remove(&(tx, ty));
            }
        }

        if let Some(entities) = sprite_index.by_chunk.remove(&coord) {
            for e in entities {
                commands.entity(e).despawn_recursive();
            }
        }
        if let Some(plant_entries) = plant_params.plant_sprite_index.by_chunk.remove(&coord) {
            for (e, tile_pos) in plant_entries {
                plant_params.plant_map.0.remove(&tile_pos);
                commands.entity(e).despawn_recursive();
            }
        }
    }

    if !*has_run {
        info!(
            "First chunk_streaming_system execution took {:?}",
            now.elapsed()
        );
        *has_run = true;
    }
}

/// PostUpdate: rebuild tile sprites at positions reported by TileChangedEvent.
pub fn refresh_changed_tiles_system(
    mut commands: Commands,
    mut events: EventReader<TileChangedEvent>,
    mut sprite_index: ResMut<TileSpriteIndex>,
    mut wall_map: ResMut<WallMap>,
    chunk_map: Res<ChunkMap>,
    tile_materials: Res<TileMaterials>,
    fog_tile_materials: Res<FogTileMaterials>,
    fog_map: Res<FogMap>,
    gen: Res<WorldGen>,
    globe: Res<Globe>,
    camera_view_z: Res<CameraViewZ>,
) {
    for ev in events.read() {
        let tx = ev.tx;
        let ty = ev.ty;
        let coord = ChunkCoord(
            (tx as i32).div_euclid(CHUNK_SIZE as i32),
            (ty as i32).div_euclid(CHUNK_SIZE as i32),
        );

        // Despawn old TileSprite entity for this position
        if let Some(old_entity) = sprite_index.by_tile.remove(&(tx, ty)) {
            if let Some(chunk_entities) = sprite_index.by_chunk.get_mut(&coord) {
                chunk_entities.retain(|&e| e != old_entity);
            }
            commands.entity(old_entity).despawn_recursive();
        }

        // Get the new tile data
        let surf_z = chunk_map.surface_z_at(tx as i32, ty as i32);
        if surf_z < Z_MIN {
            continue;
        }

        let surface_kind = chunk_map
            .tile_kind_at(tx as i32, ty as i32)
            .unwrap_or(TileKind::Air);

        // Wall entity lifecycle: only despawn when the tile is no longer a Wall
        // (e.g. mined, demolished). When the tile is still a Wall and a wall
        // entity already exists, leave it alone — the construction path already
        // attached the correct material + StructureLabel, and respawning here
        // would clobber both with a generic Stone placeholder.
        if surface_kind != TileKind::Wall {
            if let Some(wall_entity) = wall_map.0.remove(&(tx, ty)) {
                if let Some(chunk_entities) = sprite_index.by_chunk.get_mut(&coord) {
                    chunk_entities.retain(|&e| e != wall_entity);
                }
                commands.entity(wall_entity).despawn_recursive();
            }
        }

        if surface_kind == TileKind::Air {
            continue;
        }

        let wx = tx as f32 * TILE_SIZE + TILE_SIZE * 0.5;
        let wy = ty as f32 * TILE_SIZE + TILE_SIZE * 0.5;

        if surface_kind == TileKind::Wall {
            // Only spawn a placeholder for natural bedrock newly exposed
            // without an existing entity (e.g. mining adjacent rock surfaces
            // a fresh Wall tile). Constructed walls already have their
            // entity in wall_map with the correct material.
            if !wall_map.0.contains_key(&(tx, ty)) {
                let new_entity = commands
                    .spawn((
                        Wall {
                            material: WallMaterial::Stone,
                        },
                        StructureLabel(WallMaterial::Stone.label()),
                        Transform::from_xyz(wx, wy, 0.4),
                        GlobalTransform::default(),
                        Visibility::Visible,
                        InheritedVisibility::default(),
                        ProjectedAnchor::Static {
                            z: surf_z.clamp(i8::MIN as i32, i8::MAX as i32) as i8,
                        },
                        ProjectionState::default(),
                    ))
                    .id();
                wall_map.0.insert((tx, ty), new_entity);
                if let Some(chunk_entities) = sprite_index.by_chunk.get_mut(&coord) {
                    chunk_entities.push(new_entity);
                }
            }
        } else {
            let (render_kind, render_ore, render_z, base_vis) = resolve_render_tile(
                &chunk_map,
                &gen,
                &globe,
                tx as i32,
                ty as i32,
                surf_z,
                camera_view_z.0,
            );

            let tile_pos = (tx, ty);
            let mut mat =
                MeshMaterial2d(tile_materials.handle_for(render_kind, render_ore, render_z));
            let visibility = apply_fog_to_material(
                &fog_map,
                tile_pos,
                base_vis,
                render_kind,
                render_ore,
                render_z,
                &tile_materials,
                &fog_tile_materials,
                &mut mat,
            );

            let new_entity = commands
                .spawn((
                    TileSprite,
                    Mesh2d(tile_materials.tile_mesh.clone()),
                    mat,
                    Transform::from_xyz(wx, wy, 0.0),
                    GlobalTransform::default(),
                    visibility,
                    InheritedVisibility::default(),
                    ProjectedAnchor::Static {
                        z: surf_z.clamp(i8::MIN as i32, i8::MAX as i32) as i8,
                    },
                    ProjectionState::default(),
                ))
                .id();
            sprite_index.by_tile.insert((tx, ty), new_entity);
            if let Some(chunk_entities) = sprite_index.by_chunk.get_mut(&coord) {
                chunk_entities.push(new_entity);
            }
        }
    }
}

/// Update: when CameraViewZ changes, update all TileSprite materials and visibility
/// to reflect the new viewing depth.
///
/// Bevy's `is_changed()` fires on first-tick-after-insert and on any
/// `set_changed()` write — including same-value writes. The
/// `Local<Option<i32>>` guard suppresses runs where `view_z` did not
/// actually move, since this system walks the full loaded-sprite set
/// (~640K entries at LOAD_RADIUS=12) and any spurious trigger lands
/// as a visible frame stall.
pub fn update_tile_z_view_system(
    mut has_run: Local<bool>,
    mut last_view_z: Local<Option<i32>>,
    camera_view_z: Res<CameraViewZ>,
    chunk_map: Res<ChunkMap>,
    tile_materials: Res<TileMaterials>,
    fog_tile_materials: Res<FogTileMaterials>,
    fog_map: Res<FogMap>,
    gen: Res<WorldGen>,
    globe: Res<Globe>,
    sprite_index: Res<TileSpriteIndex>,
    mut query: Query<(&mut MeshMaterial2d<ColorMaterial>, &mut Visibility), With<TileSprite>>,
) {
    let view_z = camera_view_z.0;
    if *last_view_z == Some(view_z) {
        return;
    }
    *last_view_z = Some(view_z);

    let now = Instant::now();

    for (&(tx, ty), &entity) in &sprite_index.by_tile {
        let Ok((mut material, mut vis)) = query.get_mut(entity) else {
            continue;
        };

        let surf_z = chunk_map.surface_z_at(tx as i32, ty as i32);
        let (render_kind, render_ore, render_z, base_vis) = resolve_render_tile(
            &chunk_map, &gen, &globe, tx as i32, ty as i32, surf_z, view_z,
        );

        let new_vis = apply_fog_to_material(
            &fog_map,
            (tx, ty),
            base_vis,
            render_kind,
            render_ore,
            render_z,
            &tile_materials,
            &fog_tile_materials,
            &mut material,
        );
        *vis = new_vis;
    }

    if !*has_run {
        info!(
            "First update_tile_z_view_system execution took {:?}",
            now.elapsed()
        );
        *has_run = true;
    }
}
