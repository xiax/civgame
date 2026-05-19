use crate::world::chunk::{ChunkCoord, CHUNK_SIZE};
use crate::world::chunk_streaming::TileChangedEvent;
use crate::world::terrain;
use bevy::prelude::*;

pub mod astar;
pub mod chunk_graph;
pub mod chunk_router;
pub mod connectivity;
pub mod detour;
pub mod flow_field;
pub mod hotspots;
pub mod path_request;
pub mod pool;
pub mod step;
pub mod tile_cost;
pub mod worker;

pub struct PathfindingPlugin;

impl Plugin for PathfindingPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(chunk_graph::ChunkGraph::default())
            .insert_resource(chunk_graph::GraphDirty::default())
            .insert_resource(chunk_graph::GraphRebuildTask::default())
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
            // Sync full rebuild fires when ChunkMap first gets populated.
            // `terrain::spawn_world_system` runs on OnEnter(Playing) (or
            // Startup in sandbox mode via SandboxPlugin), so we mirror it
            // there. Connectivity follows immediately so `is_reachable`
            // queries on tick 1 see a coherent snapshot.
            .add_systems(
                OnEnter(crate::GameState::Playing),
                (
                    chunk_graph::startup_initial_build_system.after(terrain::spawn_world_system),
                    connectivity::rebuild_connectivity_system
                        .after(chunk_graph::startup_initial_build_system),
                ),
            )
            .add_systems(PreUpdate, chunk_graph::poll_rebuild_task_system)
            // Path drain lives on FixedUpdate so its budget scales with
            // sim speed (more `Time<Virtual>` ticks per real second at
            // higher speeds). Runs before `SimulationSet::Sequential` so
            // freshly-resolved paths land before `movement_system`
            // consumes them on the same tick.
            .add_systems(
                FixedUpdate,
                worker::drain_path_requests_system
                    .before(crate::simulation::SimulationSet::Sequential),
            )
            .add_systems(
                PostUpdate,
                (
                    invalidate_pathing_on_tile_change_system,
                    chunk_graph::enqueue_graph_dirty_system
                        .after(invalidate_pathing_on_tile_change_system),
                    chunk_graph::spawn_rebuild_task_system
                        .after(chunk_graph::enqueue_graph_dirty_system),
                    connectivity::rebuild_connectivity_system
                        .run_if(connectivity_needs_rebuild)
                        .after(chunk_graph::spawn_rebuild_task_system),
                    hotspots::rebuild_dirty_hotspots_system
                        .after(invalidate_pathing_on_tile_change_system),
                ),
            );
    }
}

/// Run condition for `rebuild_connectivity_system`. Fires whenever the
/// graph generation has advanced past connectivity's last snapshot.
/// Each `poll_rebuild_task_system` merge is atomic, so any post-merge
/// state is consistent — no need to gate on task / dirty being empty.
fn connectivity_needs_rebuild(
    graph: Res<chunk_graph::ChunkGraph>,
    conn: Res<connectivity::ChunkConnectivity>,
) -> bool {
    graph.generation != conn.generation
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
