use bevy::prelude::*;
use ahash::AHashMap;

/// Maps (tile_x, tile_y) -> list of entities occupying that tile.
/// Updated each frame by movement system.
#[derive(Resource, Default)]
pub struct SpatialIndex(pub AHashMap<(i32, i32), Vec<Entity>>);

impl SpatialIndex {
    pub fn insert(&mut self, tile_x: i32, tile_y: i32, entity: Entity) {
        self.0.entry((tile_x, tile_y)).or_default().push(entity);
    }

    pub fn remove(&mut self, tile_x: i32, tile_y: i32, entity: Entity) {
        if let Some(v) = self.0.get_mut(&(tile_x, tile_y)) {
            v.retain(|&e| e != entity);
        }
    }

    pub fn get(&self, tile_x: i32, tile_y: i32) -> &[Entity] {
        self.0.get(&(tile_x, tile_y)).map(|v| v.as_slice()).unwrap_or(&[])
    }
}
