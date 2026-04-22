use bevy::prelude::*;
use crate::world::terrain;

pub mod flow_field;
pub mod chunk_graph;

pub struct PathfindingPlugin;

impl Plugin for PathfindingPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(flow_field::FlowFieldCache::default())
            .insert_resource(chunk_graph::ChunkGraph::default())
            .add_systems(
                Startup,
                chunk_graph::build_chunk_graph_system
                    .after(terrain::spawn_world_system),
            );
    }
}
