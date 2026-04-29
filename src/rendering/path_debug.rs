use ahash::AHashSet;
use bevy::prelude::*;

use crate::pathfinding::chunk_graph::ChunkGraph;
use crate::pathfinding::chunk_router::ChunkRouter;
use crate::pathfinding::flow_field::FlowFieldCache;
use crate::rendering::camera::CameraViewZ;
use crate::simulation::movement::MovementState;
use crate::simulation::person::PersonAI;
use crate::ui::selection::SelectedEntity;
use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE};
use crate::world::terrain::{tile_to_world, TILE_SIZE};

#[derive(Resource, Default)]
pub struct PathDebugOverlay {
    pub show_selected_path: bool,
    pub show_flow_fields: bool,
    pub show_chunk_graph: bool,
}

/// Maps a flow-field direction byte (0..7) to a unit vector in tile space
/// that points *toward* the field's goal. See `flow_field::build_flow_field`
/// for the encoding (each bit is stored at a neighbor and points back at the
/// expanding cell, so the resulting arrow at that neighbor points goalward).
const DIR_VEC: [(i32, i32); 8] = [
    (0, -1),
    (1, -1),
    (1, 0),
    (1, 1),
    (0, 1),
    (-1, 1),
    (-1, 0),
    (-1, -1),
];

fn tile_center(tx: i32, ty: i32) -> Vec2 {
    tile_to_world(tx, ty)
}

fn chunk_center(coord: ChunkCoord) -> Vec2 {
    let half = CHUNK_SIZE as i32 / 2;
    tile_to_world(coord.0 * CHUNK_SIZE as i32 + half, coord.1 * CHUNK_SIZE as i32 + half)
}

pub fn selected_agent_path_gizmo_system(
    overlay: Res<PathDebugOverlay>,
    selected: Res<SelectedEntity>,
    chunk_map: Res<ChunkMap>,
    graph: Res<ChunkGraph>,
    router: Res<ChunkRouter>,
    agents: Query<(&Transform, &MovementState, &PersonAI)>,
    mut gizmos: Gizmos,
) {
    let _ = chunk_map;
    if !overlay.show_selected_path {
        return;
    }
    let Some(entity) = selected.0 else { return };
    let Ok((transform, mv, ai)) = agents.get(entity) else { return };

    let agent_pos = transform.translation.truncate();

    // Yellow A* polyline: agent → first cached step → … → final.
    let path_color = Color::srgba(1.0, 0.95, 0.2, 0.95);
    if !mv.astar_path.is_empty() {
        let cursor = (mv.astar_cursor as usize).min(mv.astar_path.len());
        let mut prev = agent_pos;
        for &(tx, ty, _z) in &mv.astar_path[cursor..] {
            let p = tile_center(tx as i32, ty as i32);
            gizmos.line_2d(prev, p, path_color);
            gizmos.circle_2d(p, 1.5, path_color);
            prev = p;
        }
        if let Some(&(tx, ty, _)) = mv.astar_path.last() {
            gizmos.circle_2d(tile_center(tx as i32, ty as i32), 4.0, path_color);
        }
    }

    // Magenta: line to the immediate sub-goal tile.
    let target_pos = tile_center(ai.target_tile.0 as i32, ai.target_tile.1 as i32);
    let target_color = Color::srgba(1.0, 0.3, 0.9, 0.85);
    gizmos.line_2d(agent_pos, target_pos, target_color);
    gizmos.circle_2d(target_pos, 3.0, target_color);

    // Cyan: line to the final destination tile (often differs from target).
    let dest_pos = tile_center(ai.dest_tile.0 as i32, ai.dest_tile.1 as i32);
    if ai.dest_tile != ai.target_tile {
        let dest_color = Color::srgba(0.3, 0.9, 1.0, 0.85);
        gizmos.line_2d(agent_pos, dest_pos, dest_color);
        gizmos.circle_2d(dest_pos, 5.0, dest_color);
    }

    // Green X: next chunk-graph waypoint (high-level routing decision).
    let cur_chunk = ChunkCoord::from_world(agent_pos.x, agent_pos.y, TILE_SIZE);
    let dest_chunk = ChunkCoord(
        (ai.dest_tile.0 as i32).div_euclid(CHUNK_SIZE as i32),
        (ai.dest_tile.1 as i32).div_euclid(CHUNK_SIZE as i32),
    );
    if cur_chunk != dest_chunk {
        if let Some((wx, wy)) =
            router.first_waypoint(&graph, cur_chunk, dest_chunk, ai.current_z)
        {
            let wp = tile_center(wx as i32, wy as i32);
            let green = Color::srgba(0.2, 1.0, 0.4, 0.95);
            let r = 4.0;
            gizmos.line_2d(wp + Vec2::new(-r, -r), wp + Vec2::new(r, r), green);
            gizmos.line_2d(wp + Vec2::new(-r, r), wp + Vec2::new(r, -r), green);
        }
    }
}

pub fn flow_field_gizmo_system(
    overlay: Res<PathDebugOverlay>,
    cache: Res<FlowFieldCache>,
    view_z: Res<CameraViewZ>,
    camera_query: Query<(&Transform, &OrthographicProjection), With<Camera>>,
    windows: Query<&Window>,
    mut gizmos: Gizmos,
) {
    if !overlay.show_flow_fields {
        return;
    }
    let Ok((cam_transform, projection)) = camera_query.get_single() else { return };
    let Ok(window) = windows.get_single() else { return };

    let half_w = window.width() * 0.5 * projection.scale;
    let half_h = window.height() * 0.5 * projection.scale;
    let cam = cam_transform.translation.truncate();
    let chunk_world = CHUNK_SIZE as f32 * TILE_SIZE;
    let cx_min = ((cam.x - half_w) / chunk_world).floor() as i32 - 1;
    let cx_max = ((cam.x + half_w) / chunk_world).ceil() as i32 + 1;
    let cy_min = ((cam.y - half_h) / chunk_world).floor() as i32 - 1;
    let cy_max = ((cam.y + half_h) / chunk_world).ceil() as i32 + 1;

    let arrow_color = Color::srgba(0.4, 0.7, 1.0, 0.55);
    let goal_color = Color::srgba(1.0, 0.25, 0.25, 0.95);
    let arrow_len = TILE_SIZE * 0.35;

    for field in cache.fields.values() {
        // View-Z filter: in surface mode (i32::MAX) show fields with goal_z >= 0;
        // when peering underground show fields whose goal_z is within ±1 of view.
        if view_z.0 == i32::MAX {
            if (field.goal_z as i32) < 0 {
                continue;
            }
        } else if (field.goal_z as i32 - view_z.0).abs() > 1 {
            continue;
        }
        let coord = field.chunk;
        if coord.0 < cx_min || coord.0 > cx_max || coord.1 < cy_min || coord.1 > cy_max {
            continue;
        }

        let origin_x = coord.0 * CHUNK_SIZE as i32;
        let origin_y = coord.1 * CHUNK_SIZE as i32;

        for ly in 0..CHUNK_SIZE {
            for lx in 0..CHUNK_SIZE {
                let dir = field.directions[ly * CHUNK_SIZE + lx];
                if dir == 0xFF || (dir as usize) >= DIR_VEC.len() {
                    continue;
                }
                let (dx, dy) = DIR_VEC[dir as usize];
                let center = tile_center(origin_x + lx as i32, origin_y + ly as i32);
                let mag = ((dx * dx + dy * dy) as f32).sqrt().max(1.0);
                let off = Vec2::new(dx as f32, dy as f32) * (arrow_len / mag);
                gizmos.line_2d(center - off * 0.5, center + off * 0.5, arrow_color);
                // Tiny tip dot so arrows read directionally.
                gizmos.circle_2d(center + off * 0.5, 0.8, arrow_color);
            }
        }

        let goal = tile_center(
            origin_x + field.goal_tile.0 as i32,
            origin_y + field.goal_tile.1 as i32,
        );
        gizmos.circle_2d(goal, 4.0, goal_color);
    }
}

pub fn chunk_graph_gizmo_system(
    overlay: Res<PathDebugOverlay>,
    graph: Res<ChunkGraph>,
    mut gizmos: Gizmos,
) {
    if !overlay.show_chunk_graph {
        return;
    }
    let node_color = Color::srgba(1.0, 0.65, 0.15, 0.85);
    let edge_color = Color::srgba(1.0, 0.65, 0.15, 0.45);

    let mut drawn: AHashSet<(ChunkCoord, ChunkCoord)> = AHashSet::new();
    for (&coord, edges) in &graph.edges {
        let a = chunk_center(coord);
        gizmos.circle_2d(a, 3.0, node_color);
        for edge in edges {
            let key = if (coord.0, coord.1) <= (edge.neighbor.0, edge.neighbor.1) {
                (coord, edge.neighbor)
            } else {
                (edge.neighbor, coord)
            };
            if !drawn.insert(key) {
                continue;
            }
            let b = chunk_center(edge.neighbor);
            gizmos.line_2d(a, b, edge_color);
        }
    }
}
