use bevy::prelude::*;
use ahash::AHashMap;

use crate::rendering::color_map::{shaded_tile_color, z_bucket};
use crate::simulation::plants::{
    spawn_plant_at, GrowthStage, PlantKind, PlantMap,
    PlantSpriteIndex,
};
use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE, Z_MIN};
use crate::world::globe::{Globe, GLOBE_CELL_CHUNKS, GLOBE_HEIGHT, GLOBE_WIDTH};
use crate::world::terrain::{generate_chunk_from_globe, tile_at_3d, WorldGen, TILE_SIZE};
use crate::world::tile::TileKind;
use crate::rendering::pixel_art::EntityTextures;

pub const LOAD_RADIUS:   i32 = 12;
pub const UNLOAD_RADIUS: i32 = 16;

// Used only for the deterministic plant hash, not for noise generation.
const PLANT_HASH_SEED: u32 = 42;

/// One ColorMaterial per (TileKind as u8, z_bucket) pair.
/// z_bucket = z.div_euclid(4), range −4..=3 → 8 buckets × 10 kinds = 80 materials.
#[derive(Resource, Default)]
pub struct TileMaterials(pub AHashMap<(u8, i32), Handle<ColorMaterial>>);

impl TileMaterials {
    pub fn handle_for(&self, kind: TileKind, z: i32) -> Handle<ColorMaterial> {
        self.0.get(&(kind as u8, z_bucket(z))).cloned().unwrap_or_default()
    }
}

#[derive(Resource, Default)]
pub struct TileSpriteIndex {
    pub by_chunk: AHashMap<ChunkCoord, Vec<Entity>>,
}

#[derive(Component)]
pub struct TileSprite;

const RENDERABLE_KINDS: &[TileKind] = &[
    TileKind::Grass, TileKind::Water, TileKind::Stone, TileKind::Forest,
    TileKind::Farmland, TileKind::Road, TileKind::Wall, TileKind::Ramp, TileKind::Dirt,
];

/// PostStartup: create one shaded ColorMaterial per (TileKind, z_bucket) pair.
pub fn setup_tile_materials(
    mut tile_materials: ResMut<TileMaterials>,
    mut materials: ResMut<Assets<ColorMaterial>>,
) {
    // z_bucket range: Z_MIN/4 to Z_MAX/4
    let bucket_min = Z_MIN.div_euclid(4);
    let bucket_max = 15_i32.div_euclid(4);

    for &kind in RENDERABLE_KINDS {
        for bucket in bucket_min..=bucket_max {
            // Representative Z for this bucket — use the midpoint.
            let z = bucket * 4 + 2;
            let color = shaded_tile_color(kind, z);
            let handle = materials.add(ColorMaterial::from_color(color));
            tile_materials.0.insert((kind as u8, bucket), handle);
        }
    }
}

/// Spawn tile sprites for a single chunk; registers them in TileSpriteIndex.
pub fn spawn_chunk_sprites(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    tile_materials: &TileMaterials,
    sprite_index: &mut TileSpriteIndex,
    chunk_map: &ChunkMap,
    coord: ChunkCoord,
) {
    let Some(chunk) = chunk_map.0.get(&coord) else { return };
    if sprite_index.by_chunk.contains_key(&coord) { return; }

    let tile_mesh = meshes.add(Rectangle::new(TILE_SIZE - 0.5, TILE_SIZE - 0.5));
    let mut entities = Vec::with_capacity(CHUNK_SIZE * CHUNK_SIZE);

    for ty in 0..CHUNK_SIZE {
        for tx in 0..CHUNK_SIZE {
            let global_tx = coord.0 * CHUNK_SIZE as i32 + tx as i32;
            let global_ty = coord.1 * CHUNK_SIZE as i32 + ty as i32;
            let surf_z = chunk.surface_z[ty][tx] as i32;
            let kind   = chunk.surface_tile_kind(tx, ty);

            // Don't render Air (open sky or void).
            if kind == TileKind::Air { continue; }

            let wx = global_tx as f32 * TILE_SIZE + TILE_SIZE * 0.5;
            let wy = global_ty as f32 * TILE_SIZE + TILE_SIZE * 0.5;

            let entity = commands.spawn((
                TileSprite,
                Mesh2d(tile_mesh.clone()),
                MeshMaterial2d(tile_materials.handle_for(kind, surf_z)),
                Transform::from_xyz(wx, wy, 0.0),
            )).id();
            entities.push(entity);
        }
    }

    sprite_index.by_chunk.insert(coord, entities);
}

/// Deterministically seed initial plants for a chunk using a hash of tile coordinates.
pub fn spawn_chunk_plants(
    commands: &mut Commands,
    plant_map: &mut PlantMap,
    plant_sprite_index: &mut PlantSpriteIndex,
    textures: &EntityTextures,
    chunk_map: &ChunkMap,
    gen: &WorldGen,
    globe: &Globe,
    coord: ChunkCoord,
) {
    let Some(chunk) = chunk_map.0.get(&coord) else { return };

    for ty in 0..CHUNK_SIZE {
        for tx in 0..CHUNK_SIZE {
            let global_tx = coord.0 * CHUNK_SIZE as i32 + tx as i32;
            let global_ty = coord.1 * CHUNK_SIZE as i32 + ty as i32;
            let surf_z = chunk.surface_z[ty][tx] as i32;
            let tile = tile_at_3d(chunk_map, gen, globe, global_tx, global_ty, surf_z);

            // Deterministic hash for this tile
            let h = (global_tx.wrapping_mul(2_654_435_761_u32 as i32)
                ^ global_ty.wrapping_mul(2_246_822_519_u32 as i32)
                ^ PLANT_HASH_SEED as i32) as u32;

            match tile.kind {
                TileKind::Farmland => {
                    let pct = h % 100;
                    let (kind, stage) = if pct < 5 {
                        (PlantKind::FruitBush, initial_stage(h))
                    } else if pct < 15 {
                        (PlantKind::Grain, initial_stage(h))
                    } else {
                        continue;
                    };
                    spawn_plant_at(
                        commands, plant_map, plant_sprite_index,
                        textures,
                        global_tx, global_ty, kind, stage,
                    );
                }
                TileKind::Grass if tile.fertility > 120 => {
                    if h % 100 < 2 {
                        spawn_plant_at(
                            commands, plant_map, plant_sprite_index,
                            textures,
                            global_tx, global_ty, PlantKind::FruitBush, initial_stage(h),
                        );
                    }
                }
                TileKind::Forest => {
                    if h % 100 < 10 {
                        spawn_plant_at(
                            commands, plant_map, plant_sprite_index,
                            textures,
                            global_tx, global_ty, PlantKind::Tree, initial_stage(h),
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

/// PostStartup: spawn sprites for initial chunks (already generated by spawn_world_system).
pub fn spawn_initial_tile_sprites(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    tile_materials: Res<TileMaterials>,
    mut sprite_index: ResMut<TileSpriteIndex>,
    chunk_map: Res<ChunkMap>,
    gen: Res<WorldGen>,
    mut globe: ResMut<Globe>,
    mut plant_map: ResMut<PlantMap>,
    mut plant_sprite_index: ResMut<PlantSpriteIndex>,
    textures: Res<EntityTextures>,
) {
    let coords: Vec<ChunkCoord> = chunk_map.0.keys().copied().collect();
    for coord in coords {
        let (gx, gy) = Globe::cell_for_chunk(coord.0, coord.1);
        if let Some(gc) = globe.cell_mut(gx, gy) {
            gc.explored = true;
        }

        spawn_chunk_sprites(
            &mut commands,
            &mut meshes,
            &tile_materials,
            &mut sprite_index,
            &chunk_map,
            coord,
        );

        spawn_chunk_plants(
            &mut commands,
            &mut plant_map,
            &mut plant_sprite_index,
            &textures,
            &chunk_map,
            &gen,
            &globe,
            coord,
        );
    }

    info!("Initial tile sprites spawned for {} chunks", sprite_index.by_chunk.len());
}

/// Update: stream chunks in/out as the camera moves.
pub fn chunk_streaming_system(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    tile_materials: Res<TileMaterials>,
    mut sprite_index: ResMut<TileSpriteIndex>,
    mut chunk_map: ResMut<ChunkMap>,
    gen: Res<WorldGen>,
    mut globe: ResMut<Globe>,
    mut plant_map: ResMut<PlantMap>,
    mut plant_sprite_index: ResMut<PlantSpriteIndex>,
    textures: Res<EntityTextures>,
    camera_q: Query<&Transform, With<Camera>>,
) {
    let Ok(cam_transform) = camera_q.get_single() else { return };

    let cam_cx = (cam_transform.translation.x / (CHUNK_SIZE as f32 * TILE_SIZE)).floor() as i32;
    let cam_cy = (cam_transform.translation.y / (CHUNK_SIZE as f32 * TILE_SIZE)).floor() as i32;

    let total_cx = GLOBE_WIDTH  * GLOBE_CELL_CHUNKS;
    let total_cy = GLOBE_HEIGHT * GLOBE_CELL_CHUNKS;

    // --- Load chunks within LOAD_RADIUS ---
    for dy in -LOAD_RADIUS..=LOAD_RADIUS {
        for dx in -LOAD_RADIUS..=LOAD_RADIUS {
            let cx = cam_cx + dx;
            let cy = cam_cy + dy;

            if cx < 0 || cy < 0 || cx >= total_cx || cy >= total_cy { continue; }

            let coord = ChunkCoord(cx, cy);
            if chunk_map.0.contains_key(&coord) { continue; }

            let (gx, gy) = Globe::cell_for_chunk(cx, cy);
            let cell = globe.cell(gx, gy).copied();
            let Some(c) = cell else { continue };
            let chunk = generate_chunk_from_globe(coord, &c, &gen);
            chunk_map.0.insert(coord, chunk);

            if let Some(gc) = globe.cell_mut(gx, gy) {
                gc.explored = true;
            }

            spawn_chunk_sprites(
                &mut commands,
                &mut meshes,
                &tile_materials,
                &mut sprite_index,
                &chunk_map,
                coord,
            );

            spawn_chunk_plants(
                &mut commands,
                &mut plant_map,
                &mut plant_sprite_index,
                &textures,
                &chunk_map,
                &gen,
                &globe,
                coord,
            );
        }
    }

    // --- Unload chunks beyond UNLOAD_RADIUS ---
    let to_unload: Vec<ChunkCoord> = chunk_map.0.keys()
        .copied()
        .filter(|&c| {
            let dx = (c.0 - cam_cx).abs();
            let dy = (c.1 - cam_cy).abs();
            dx.max(dy) > UNLOAD_RADIUS
        })
        .collect();

    for coord in to_unload {
        chunk_map.0.remove(&coord);
        if let Some(entities) = sprite_index.by_chunk.remove(&coord) {
            for e in entities {
                commands.entity(e).despawn();
            }
        }
        if let Some(plant_entries) = plant_sprite_index.by_chunk.remove(&coord) {
            for (e, tile_pos) in plant_entries {
                plant_map.0.remove(&tile_pos);
                commands.entity(e).despawn();
            }
        }
    }
}
