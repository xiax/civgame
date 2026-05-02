use ahash::{AHashMap, AHashSet};
use bevy::ecs::system::SystemParam;
use bevy::prelude::*;
use std::time::Instant;

use crate::pathfinding::path_request::PathFollow;
use crate::rendering::camera::CameraViewZ;
use crate::rendering::color_map::{shaded_ore_tile_color, shaded_tile_color, z_bucket};
use crate::rendering::fog::{apply_fog_to_material, FogMap, FogTileMaterials};
use crate::simulation::construction::{Wall, WallMap, WallMaterial};
use crate::simulation::faction::{FactionCenter, StorageTileMap};
use crate::simulation::plants::{
    spawn_plant_at, GrowthStage, PlantKind, PlantMap, PlantSpriteIndex,
};
use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE, Z_MIN};
use crate::world::globe::{Globe, GLOBE_CELL_CHUNKS, GLOBE_HEIGHT, GLOBE_WIDTH};
use crate::world::terrain::{generate_chunk_from_globe, tile_at_3d, WorldGen, TILE_SIZE};
use crate::world::tile::{OreKind, TileKind};

pub const LOAD_RADIUS: i32 = 12;
pub const UNLOAD_RADIUS: i32 = 16;

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

/// Bundles the load/unload event writers so `chunk_streaming_system` stays
/// under Bevy's 16-parameter system limit.
#[derive(SystemParam)]
pub struct ChunkStreamEvents<'w> {
    pub loaded: EventWriter<'w, ChunkLoadedEvent>,
    pub unloaded: EventWriter<'w, ChunkUnloadedEvent>,
}

/// Runs each tick before `chunk_streaming_system`. Rebuilds `ChunkRetention`
/// from FactionCenter / StorageTileMap / PathFollow. Cheap — bounded by
/// (factions + storage tiles + active path lengths), well under a millisecond
/// in practice.
pub fn update_chunk_retention_system(
    mut retention: ResMut<ChunkRetention>,
    storage: Res<StorageTileMap>,
    centers: Query<&Transform, With<FactionCenter>>,
    follows: Query<&PathFollow>,
) {
    retention.pinned.clear();

    for transform in &centers {
        let coord = chunk_coord_from_world(transform.translation.x, transform.translation.y);
        retention.pinned.insert(coord);
    }

    for &(tx, ty) in storage.tiles.keys() {
        retention.pinned.insert(ChunkCoord(
            (tx as i32).div_euclid(CHUNK_SIZE as i32),
            (ty as i32).div_euclid(CHUNK_SIZE as i32),
        ));
    }

    for follow in &follows {
        for &coord in &follow.chunk_route {
            retention.pinned.insert(coord);
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
    pub by_tile: AHashMap<(i16, i16), Entity>,
}

#[derive(Component)]
pub struct TileSprite;

/// Fired by dig_system when a tile's surface changes. The rendering layer
/// despawns the old sprite and spawns a new one matching the updated terrain.
#[derive(Event)]
pub struct TileChangedEvent {
    pub tx: i16,
    pub ty: i16,
}

pub const RENDERABLE_KINDS: &[TileKind] = &[
    TileKind::Grass,
    TileKind::Water,
    TileKind::Stone,
    TileKind::Forest,
    TileKind::Farmland,
    TileKind::Road,
    TileKind::Wall,
    TileKind::Ramp,
    TileKind::Dirt,
    TileKind::Ore,
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
            let tile_pos = (global_tx as i16, global_ty as i16);

            if kind == TileKind::Wall {
                if !wall_map.0.contains_key(&tile_pos) {
                    let entity = commands
                        .spawn((
                            Wall {
                                material: WallMaterial::Stone,
                            },
                            Transform::from_xyz(wx, wy, 0.4),
                            GlobalTransform::default(),
                            Visibility::Visible,
                            InheritedVisibility::default(),
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
                ))
                .id();
            entities.push(entity);
            sprite_index.by_tile.insert(tile_pos, entity);
        }
    }

    sprite_index.by_chunk.insert(coord, entities);
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
        (TileKind::Grass, OreKind::None, camera_view_z, Visibility::Hidden)
    } else {
        (
            underground_tile.kind,
            underground_tile.ore_kind(),
            camera_view_z,
            Visibility::Visible,
        )
    }
}

/// Deterministically seed initial plants for a chunk.
pub fn spawn_chunk_plants(
    commands: &mut Commands,
    plant_map: &mut PlantMap,
    plant_sprite_index: &mut PlantSpriteIndex,
    chunk_map: &ChunkMap,
    gen: &WorldGen,
    globe: &Globe,
    coord: ChunkCoord,
) {
    let Some(chunk) = chunk_map.0.get(&coord) else {
        return;
    };

    for ty in 0..CHUNK_SIZE {
        for tx in 0..CHUNK_SIZE {
            let global_tx = coord.0 * CHUNK_SIZE as i32 + tx as i32;
            let global_ty = coord.1 * CHUNK_SIZE as i32 + ty as i32;
            let surf_z = chunk.surface_z[ty][tx] as i32;
            let tile = tile_at_3d(chunk_map, gen, globe, global_tx, global_ty, surf_z);

            let h = (global_tx.wrapping_mul(2_654_435_761_u32 as i32)
                ^ global_ty.wrapping_mul(2_246_822_519_u32 as i32)
                ^ PLANT_HASH_SEED as i32) as u32;

            match tile.kind {
                TileKind::Farmland => {
                    let pct = h % 100;
                    let (kind, stage) = if pct < 5 {
                        (PlantKind::BerryBush, initial_stage(h))
                    } else if pct < 15 {
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
                TileKind::Grass if tile.fertility > 100 => {
                    let pct = h % 100;
                    if pct < 2 {
                        spawn_plant_at(
                            commands,
                            plant_map,
                            plant_sprite_index,
                            global_tx,
                            global_ty,
                            PlantKind::BerryBush,
                            initial_stage(h),
                        );
                    } else if pct < 7 {
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
    mut plant_map: ResMut<PlantMap>,
    mut plant_sprite_index: ResMut<PlantSpriteIndex>,
    camera_view_z: Res<CameraViewZ>,
    retention: Res<ChunkRetention>,
    mut stream_events: ChunkStreamEvents,
    camera_q: Query<&Transform, With<Camera>>,
) {
    let now = Instant::now();
    let Ok(cam_transform) = camera_q.get_single() else {
        return;
    };

    let cam_cx = (cam_transform.translation.x / (CHUNK_SIZE as f32 * TILE_SIZE)).floor() as i32;
    let cam_cy = (cam_transform.translation.y / (CHUNK_SIZE as f32 * TILE_SIZE)).floor() as i32;

    let total_cx = GLOBE_WIDTH * GLOBE_CELL_CHUNKS;
    let total_cy = GLOBE_HEIGHT * GLOBE_CELL_CHUNKS;

    // --- Load chunks within LOAD_RADIUS ---
    for dy in -LOAD_RADIUS..=LOAD_RADIUS {
        for dx in -LOAD_RADIUS..=LOAD_RADIUS {
            let cx = cam_cx + dx;
            let cy = cam_cy + dy;

            if cx < 0 || cy < 0 || cx >= total_cx || cy >= total_cy {
                continue;
            }

            let coord = ChunkCoord(cx, cy);
            let (gx, gy) = Globe::cell_for_chunk(cx, cy);

            // 1. Ensure the chunk data exists in ChunkMap
            if !chunk_map.0.contains_key(&coord) {
                let cell = globe.cell(gx, gy).copied();
                let Some(c) = cell else { continue };
                let chunk = generate_chunk_from_globe(coord, &c, &gen);
                chunk_map.0.insert(coord, chunk);
                stream_events.loaded.send(ChunkLoadedEvent { coord });

                if let Some(gc) = globe.cell_mut(gx, gy) {
                    gc.explored = true;
                }
            }

            // 2. Lazy-spawn sprites if not already present
            if !sprite_index.by_chunk.contains_key(&coord) {
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
                );

                spawn_chunk_plants(
                    &mut commands,
                    &mut plant_map,
                    &mut plant_sprite_index,
                    &chunk_map,
                    &gen,
                    &globe,
                    coord,
                );
            }
        }
    }

    // --- Unload chunks beyond UNLOAD_RADIUS ---
    // Pinned chunks (faction centers, storage tiles, active path routes) are
    // exempt — see `ChunkRetention` for why.
    let to_unload: Vec<ChunkCoord> = chunk_map
        .0
        .keys()
        .copied()
        .filter(|&c| {
            if retention.pinned.contains(&c) {
                return false;
            }
            let dx = (c.0 - cam_cx).abs();
            let dy = (c.1 - cam_cy).abs();
            dx.max(dy) > UNLOAD_RADIUS
        })
        .collect();

    for coord in to_unload {
        chunk_map.0.remove(&coord);
        stream_events.unloaded.send(ChunkUnloadedEvent { coord });

        let x0 = (coord.0 * CHUNK_SIZE as i32) as i16;
        let y0 = (coord.1 * CHUNK_SIZE as i32) as i16;

        // Optimization: iterate locally over chunk tiles instead of scanning the whole map.
        for ly in 0..CHUNK_SIZE as i16 {
            for lx in 0..CHUNK_SIZE as i16 {
                let tx = x0 + lx;
                let ty = y0 + ly;
                wall_map.0.remove(&(tx, ty));
                sprite_index.by_tile.remove(&(tx, ty));
            }
        }

        if let Some(entities) = sprite_index.by_chunk.remove(&coord) {
            for e in entities {
                commands.entity(e).despawn_recursive();
            }
        }
        if let Some(plant_entries) = plant_sprite_index.by_chunk.remove(&coord) {
            for (e, tile_pos) in plant_entries {
                plant_map.0.remove(&tile_pos);
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

        // Also clean up any Wall entity at this position (e.g., mined wall)
        if let Some(wall_entity) = wall_map.0.remove(&(tx, ty)) {
            if let Some(chunk_entities) = sprite_index.by_chunk.get_mut(&coord) {
                chunk_entities.retain(|&e| e != wall_entity);
            }
            commands.entity(wall_entity).despawn_recursive();
        }

        // Get the new tile data
        let surf_z = chunk_map.surface_z_at(tx as i32, ty as i32);
        if surf_z < Z_MIN {
            continue;
        }

        let surface_kind = chunk_map
            .tile_kind_at(tx as i32, ty as i32)
            .unwrap_or(TileKind::Air);
        if surface_kind == TileKind::Air {
            continue;
        }

        let wx = tx as f32 * TILE_SIZE + TILE_SIZE * 0.5;
        let wy = ty as f32 * TILE_SIZE + TILE_SIZE * 0.5;

        if surface_kind == TileKind::Wall {
            // Spawn a new Wall entity (entity_sprites will attach the visual child)
            let new_entity = commands
                .spawn((
                    Wall {
                        material: WallMaterial::Stone,
                    },
                    Transform::from_xyz(wx, wy, 0.4),
                    GlobalTransform::default(),
                    Visibility::Visible,
                    InheritedVisibility::default(),
                ))
                .id();
            wall_map.0.insert((tx, ty), new_entity);
            if let Some(chunk_entities) = sprite_index.by_chunk.get_mut(&coord) {
                chunk_entities.push(new_entity);
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
pub fn update_tile_z_view_system(
    mut has_run: Local<bool>,
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
    if !camera_view_z.is_changed() {
        return;
    }

    let now = Instant::now();
    let view_z = camera_view_z.0;

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
