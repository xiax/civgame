use bevy::prelude::*;
use ahash::AHashMap;
use noise::{Perlin, Seedable};

use crate::rendering::color_map::tile_color;
use crate::simulation::plants::{
    spawn_plant_at, GrowthStage, PlantKind, PlantMap,
    PlantSpriteIndex,
};
use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE};
use crate::world::globe::{Globe, GLOBE_CELL_CHUNKS, GLOBE_HEIGHT, GLOBE_WIDTH};
use crate::world::terrain::{generate_chunk_from_globe, TILE_SIZE};
use crate::world::tile::TileKind;
use crate::rendering::pixel_art::EntityTextures;

pub const LOAD_RADIUS:   i32 = 12;
pub const UNLOAD_RADIUS: i32 = 16;

const WORLD_SEED: u32 = 42;

#[derive(Resource, Default)]
pub struct TileMaterials {
    pub grass:    Handle<ColorMaterial>,
    pub water:    Handle<ColorMaterial>,
    pub stone:    Handle<ColorMaterial>,
    pub forest:   Handle<ColorMaterial>,
    pub farmland: Handle<ColorMaterial>,
    pub road:     Handle<ColorMaterial>,
}

impl TileMaterials {
    pub fn handle_for(&self, kind: TileKind) -> Handle<ColorMaterial> {
        match kind {
            TileKind::Grass    => self.grass.clone(),
            TileKind::Water    => self.water.clone(),
            TileKind::Stone    => self.stone.clone(),
            TileKind::Forest   => self.forest.clone(),
            TileKind::Farmland => self.farmland.clone(),
            TileKind::Road     => self.road.clone(),
        }
    }
}

#[derive(Resource, Default)]
pub struct TileSpriteIndex {
    pub by_chunk: AHashMap<ChunkCoord, Vec<Entity>>,
}

#[derive(Component)]
pub struct TileSprite;

/// PostStartup: create one shared ColorMaterial per TileKind.
pub fn setup_tile_materials(
    mut tile_materials: ResMut<TileMaterials>,
    mut materials: ResMut<Assets<ColorMaterial>>,
) {
    tile_materials.grass    = materials.add(ColorMaterial::from_color(tile_color(TileKind::Grass)));
    tile_materials.water    = materials.add(ColorMaterial::from_color(tile_color(TileKind::Water)));
    tile_materials.stone    = materials.add(ColorMaterial::from_color(tile_color(TileKind::Stone)));
    tile_materials.forest   = materials.add(ColorMaterial::from_color(tile_color(TileKind::Forest)));
    tile_materials.farmland = materials.add(ColorMaterial::from_color(tile_color(TileKind::Farmland)));
    tile_materials.road     = materials.add(ColorMaterial::from_color(tile_color(TileKind::Road)));
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
            let tile = chunk.tile(tx, ty);
            let global_tx = coord.0 * CHUNK_SIZE as i32 + tx as i32;
            let global_ty = coord.1 * CHUNK_SIZE as i32 + ty as i32;
            let wx = global_tx as f32 * TILE_SIZE + TILE_SIZE * 0.5;
            let wy = global_ty as f32 * TILE_SIZE + TILE_SIZE * 0.5;

            let entity = commands.spawn((
                TileSprite,
                Mesh2d(tile_mesh.clone()),
                MeshMaterial2d(tile_materials.handle_for(tile.kind)),
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
    coord: ChunkCoord,
) {
    let Some(chunk) = chunk_map.0.get(&coord) else { return };

    for ty in 0..CHUNK_SIZE {
        for tx in 0..CHUNK_SIZE {
            let tile = chunk.tile(tx, ty);
            let global_tx = coord.0 * CHUNK_SIZE as i32 + tx as i32;
            let global_ty = coord.1 * CHUNK_SIZE as i32 + ty as i32;

            // Deterministic hash for this tile
            let h = (global_tx.wrapping_mul(2_654_435_761_u32 as i32)
                ^ global_ty.wrapping_mul(2_246_822_519_u32 as i32)
                ^ WORLD_SEED as i32) as u32;

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
    mut globe: ResMut<Globe>,
    mut plant_map: ResMut<PlantMap>,
    mut plant_sprite_index: ResMut<PlantSpriteIndex>,
    textures: Res<EntityTextures>,
) {
    let coords: Vec<ChunkCoord> = chunk_map.0.keys().copied().collect();
    for coord in coords {
        // Mark globe cell explored
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
    mut globe: ResMut<Globe>,
    mut plant_map: ResMut<PlantMap>,
    mut plant_sprite_index: ResMut<PlantSpriteIndex>,
    textures: Res<EntityTextures>,
    camera_q: Query<&Transform, With<Camera>>,
) {
    let Ok(cam_transform) = camera_q.get_single() else { return };

    let cam_cx = (cam_transform.translation.x / (CHUNK_SIZE as f32 * TILE_SIZE)).floor() as i32;
    let cam_cy = (cam_transform.translation.y / (CHUNK_SIZE as f32 * TILE_SIZE)).floor() as i32;

    let perlin = Perlin::default().set_seed(WORLD_SEED);

    // --- Load chunks within LOAD_RADIUS that aren't loaded yet ---
    let total_cx = GLOBE_WIDTH  * GLOBE_CELL_CHUNKS;
    let total_cy = GLOBE_HEIGHT * GLOBE_CELL_CHUNKS;

    for dy in -LOAD_RADIUS..=LOAD_RADIUS {
        for dx in -LOAD_RADIUS..=LOAD_RADIUS {
            let cx = cam_cx + dx;
            let cy = cam_cy + dy;

            if cx < 0 || cy < 0 || cx >= total_cx || cy >= total_cy { continue; }

            let coord = ChunkCoord(cx, cy);
            if chunk_map.0.contains_key(&coord) { continue; }

            let (gx, gy) = Globe::cell_for_chunk(cx, cy);
            let cell = globe.cell(gx, gy).copied();
            let chunk = match cell {
                Some(c) => generate_chunk_from_globe(coord, &c, &perlin),
                None    => continue,
            };
            chunk_map.0.insert(coord, chunk);

            // Mark globe cell as explored
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
        // Unload plants
        if let Some(plant_entries) = plant_sprite_index.by_chunk.remove(&coord) {
            for (e, tile_pos) in plant_entries {
                plant_map.0.remove(&tile_pos);
                commands.entity(e).despawn();
            }
        }
    }
}
