use crate::world::chunk::{ChunkCoord, CHUNK_SIZE};
use crate::world::chunk_streaming::TileChangedEvent;
use crate::world::terrain;
use bevy::prelude::*;

pub mod astar;
pub mod chunk_graph;
pub mod chunk_router;
pub mod connectivity;
pub mod flow_field;
pub mod hotspots;
pub mod path_request;
pub mod pool;
pub mod tile_cost;
pub mod worker;

pub struct PathfindingPlugin;

impl Plugin for PathfindingPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(chunk_graph::ChunkGraph::default())
            .insert_resource(connectivity::ChunkConnectivity::default())
            .insert_resource(chunk_router::ChunkRouter::default())
            .insert_resource(hotspots::HotspotFlowFields::default())
            .insert_resource(pool::AStarPool::default())
            .insert_resource(path_request::PathRequestQueue::default())
            .insert_resource(path_request::PathDebugFlags::default())
            .insert_resource(path_request::FailureLog::default())
            .insert_resource(worker::PathfindingDiagnostics::default())
            .add_event::<path_request::PathReady>()
            .add_event::<path_request::PathFailed>()
            .add_systems(
                Startup,
                (
                    chunk_graph::build_chunk_graph_system.after(terrain::spawn_world_system),
                    connectivity::rebuild_connectivity_system
                        .after(chunk_graph::build_chunk_graph_system),
                ),
            )
            .add_systems(PreUpdate, worker::drain_path_requests_system)
            .add_systems(
                PostUpdate,
                (
                    invalidate_pathing_on_tile_change_system,
                    chunk_graph::build_chunk_graph_system
                        .run_if(bevy::ecs::schedule::common_conditions::on_event::<TileChangedEvent>)
                        .after(invalidate_pathing_on_tile_change_system),
                    connectivity::rebuild_connectivity_system
                        .run_if(bevy::ecs::schedule::common_conditions::on_event::<TileChangedEvent>)
                        .after(chunk_graph::build_chunk_graph_system),
                    hotspots::rebuild_dirty_hotspots_system
                        .after(invalidate_pathing_on_tile_change_system),
                ),
            );
    }
}

/// Drops hotspot flow fields for any chunk touched by a `TileChangedEvent`
/// (and its 8 neighbors, since a flow field built in chunk A can route
/// through border tiles of chunk B). Runs in `PostUpdate` ahead of the
/// chunk-graph rebuild and the hotspot rebuild.
fn invalidate_pathing_on_tile_change_system(
    mut events: EventReader<TileChangedEvent>,
    mut hotspots: ResMut<hotspots::HotspotFlowFields>,
) {
    for ev in events.read() {
        let coord = ChunkCoord(
            (ev.tx as i32).div_euclid(CHUNK_SIZE as i32),
            (ev.ty as i32).div_euclid(CHUNK_SIZE as i32),
        );
        hotspots.invalidate_chunk(coord);
        for (dx, dy) in &[
            (-1, -1),
            (-1, 0),
            (-1, 1),
            (0, -1),
            (0, 1),
            (1, -1),
            (1, 0),
            (1, 1),
        ] {
            hotspots.invalidate_chunk(ChunkCoord(coord.0 + dx, coord.1 + dy));
        }
    }
}
