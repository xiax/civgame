use crate::rendering::camera::CameraState;
use crate::world::chunk::ChunkCoord;
use crate::world::terrain::TILE_SIZE;
use bevy::prelude::*;

#[repr(u8)]
#[derive(Component, Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum LodLevel {
    #[default]
    Full = 0,
    Aggregate = 1,
    Dormant = 2,
}

pub fn update_lod_levels_system(
    camera: Query<&Transform, With<Camera>>,
    camera_state: Res<CameraState>,
    mut entities: Query<(&Transform, &mut LodLevel)>,
) {
    let Ok(cam_transform) = camera.get_single() else {
        return;
    };
    let cam_pos = cam_transform.translation.truncate();
    let cam_chunk = ChunkCoord::from_world(cam_pos.x, cam_pos.y, TILE_SIZE);

    // Adjust LOD thresholds based on zoom
    let zoom = camera_state.zoom;
    let full_dist = if zoom < 4.0 { 4 } else { 2 };
    let agg_dist = if zoom < 4.0 { 12 } else { 6 };

    entities.par_iter_mut().for_each(|(transform, mut lod)| {
        let pos = transform.translation.truncate();
        let entity_chunk = ChunkCoord::from_world(pos.x, pos.y, TILE_SIZE);
        let dist = cam_chunk.chebyshev_dist(entity_chunk);
        *lod = match dist {
            d if d <= full_dist => LodLevel::Full,
            d if d <= agg_dist => LodLevel::Aggregate,
            _ => LodLevel::Dormant,
        };
    });
}
