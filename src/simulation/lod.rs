use crate::rendering::camera::CameraState;
use crate::simulation::region::SimulationFocus;
use crate::world::chunk::{ChunkCoord, CHUNK_SIZE};
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

/// LOD is computed against the nearest `SimulationFocus` point — camera or
/// any settled region centre — so off-camera settled regions don't all drop
/// to `Dormant` whenever the camera moves. Camera focus uses the original
/// zoom-aware thresholds (Full/Aggregate by chunk distance); off-camera
/// region focus collapses to a fixed Aggregate band.
pub fn update_lod_levels_system(
    camera: Query<&Transform, With<Camera>>,
    camera_state: Res<CameraState>,
    focus: Res<SimulationFocus>,
    mut entities: Query<(&Transform, &mut LodLevel)>,
) {
    let zoom = camera_state.zoom;
    let full_dist = if zoom < 4.0 { 4 } else { 2 };
    let agg_dist = if zoom < 4.0 { 12 } else { 6 };

    // Pre-compute focus chunk centres.
    let cam_chunk = camera.get_single().ok().map(|t| {
        let p = t.translation.truncate();
        ChunkCoord::from_world(p.x, p.y, TILE_SIZE)
    });
    let region_chunks: Vec<ChunkCoord> = focus
        .points
        .iter()
        .filter(|p| !p.is_camera)
        .map(|p| {
            let cx = (p.world_pos.x / (CHUNK_SIZE as f32 * TILE_SIZE)).floor() as i32;
            let cy = (p.world_pos.y / (CHUNK_SIZE as f32 * TILE_SIZE)).floor() as i32;
            ChunkCoord(cx, cy)
        })
        .collect();

    entities.par_iter_mut().for_each(|(transform, mut lod)| {
        let pos = transform.translation.truncate();
        let entity_chunk = ChunkCoord::from_world(pos.x, pos.y, TILE_SIZE);

        let cam_dist = cam_chunk
            .map(|c| c.chebyshev_dist(entity_chunk))
            .unwrap_or(i32::MAX);

        // Camera-relative LOD: Full near camera, Aggregate further, Dormant beyond.
        let cam_lod = match cam_dist {
            d if d <= full_dist => LodLevel::Full,
            d if d <= agg_dist => LodLevel::Aggregate,
            _ => LodLevel::Dormant,
        };

        // If the entity is also near a settled-region focus, promote it from
        // Dormant to at least Aggregate so off-camera sim runs.
        let near_region = region_chunks
            .iter()
            .any(|c| c.chebyshev_dist(entity_chunk) <= 8);
        *lod = match (cam_lod, near_region) {
            (LodLevel::Dormant, true) => LodLevel::Aggregate,
            (other, _) => other,
        };
    });
}
