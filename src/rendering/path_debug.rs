use ahash::AHashSet;
use bevy::prelude::*;

use crate::pathfinding::chunk_graph::ChunkGraph;
use crate::pathfinding::connectivity::ChunkConnectivity;
use crate::pathfinding::hotspots::HotspotFlowFields;
use crate::pathfinding::path_request::{FailureLog, PathFollow};
use crate::rendering::camera::CameraViewZ;
use crate::simulation::person::PersonAI;
use crate::ui::selection::SelectedEntity;
use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE};
use crate::world::terrain::{tile_to_world, TILE_SIZE};

#[derive(Resource, Default)]
pub struct PathDebugOverlay {
    pub show_selected_path: bool,
    pub show_flow_fields: bool,
    pub show_chunk_graph: bool,
    /// Red lines for the most recent global PathFailed (start → goal).
    pub show_recent_failures: bool,
    /// Tints chunks by their connectivity component id, so islands stand out.
    pub show_connectivity_components: bool,
    /// Failure history specific to the currently selected agent.
    pub show_selected_failures: bool,
}

/// Maps a flow-field direction byte (0..7) to a unit vector in tile space
/// that points *toward* the field's goal. See `flow_field::build_flow_field`
/// for the encoding: byte `d` is stored at a neighbor whose offset from its
/// parent is `flow_field::DIR_OFFSET[d]`, so walking goalward from that
/// neighbor means stepping in the *opposite* direction. This table is
/// `-DIR_OFFSET[d]` element-wise — keep them in sync.
const DIR_VEC: [(i32, i32); 8] = [
    (0, 1),
    (1, 1),
    (1, 0),
    (1, -1),
    (0, -1),
    (-1, -1),
    (-1, 0),
    (-1, 1),
];

fn tile_center(tx: i32, ty: i32) -> Vec2 {
    tile_to_world(tx, ty)
}

fn chunk_center(coord: ChunkCoord) -> Vec2 {
    let half = CHUNK_SIZE as i32 / 2;
    tile_to_world(
        coord.0 * CHUNK_SIZE as i32 + half,
        coord.1 * CHUNK_SIZE as i32 + half,
    )
}

pub fn selected_agent_path_gizmo_system(
    overlay: Res<PathDebugOverlay>,
    selected: Res<SelectedEntity>,
    chunk_map: Res<ChunkMap>,
    agents: Query<(&Transform, &PathFollow, &PersonAI)>,
    mut gizmos: Gizmos,
) {
    let _ = chunk_map;
    if !overlay.show_selected_path {
        return;
    }
    let Some(entity) = selected.0 else { return };
    let Ok((transform, pf, ai)) = agents.get(entity) else {
        return;
    };

    let agent_pos = transform.translation.truncate();

    // Yellow polyline: current PathFollow segment, agent → step 0 → … → segment end.
    let path_color = Color::srgba(1.0, 0.95, 0.2, 0.95);
    if !pf.segment_path.is_empty() {
        let cursor = (pf.segment_cursor as usize).min(pf.segment_path.len());
        let mut prev = agent_pos;
        for &(tx, ty, _z) in &pf.segment_path[cursor..] {
            let p = tile_center(tx as i32, ty as i32);
            gizmos.line_2d(prev, p, path_color);
            gizmos.circle_2d(p, 1.5, path_color);
            prev = p;
        }
        if let Some(&(tx, ty, _)) = pf.segment_path.last() {
            gizmos.circle_2d(tile_center(tx as i32, ty as i32), 4.0, path_color);
        }
    }

    // Magenta: line to the immediate target tile.
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

    // Green X chain: chunks the worker plans to traverse, in order.
    let chunk_color = Color::srgba(0.2, 1.0, 0.4, 0.95);
    let r = 4.0;
    for (i, coord) in pf.chunk_route.iter().enumerate() {
        if (i as u8) < pf.route_cursor {
            continue;
        }
        let c = chunk_center(*coord);
        gizmos.line_2d(c + Vec2::new(-r, -r), c + Vec2::new(r, r), chunk_color);
        gizmos.line_2d(c + Vec2::new(-r, r), c + Vec2::new(r, -r), chunk_color);
    }
}

pub fn flow_field_gizmo_system(
    overlay: Res<PathDebugOverlay>,
    hotspots: Res<HotspotFlowFields>,
    view_z: Res<CameraViewZ>,
    camera_query: Query<(&Transform, &OrthographicProjection), With<Camera>>,
    windows: Query<&Window>,
    mut gizmos: Gizmos,
) {
    if !overlay.show_flow_fields {
        return;
    }
    let Ok((cam_transform, projection)) = camera_query.get_single() else {
        return;
    };
    let Ok(window) = windows.get_single() else {
        return;
    };

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

    for entry in hotspots.entries.values() {
        let field = &entry.field;
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

/// Cheap deterministic hash → hue mapping for component ids. Same id ⇒
/// same color across frames, but adjacent ids get visually distinct hues.
fn component_color(id: u32) -> Color {
    // Mix the id with golden-ratio constant to spread hues.
    let h = ((id.wrapping_mul(2654435761)) >> 16) as f32 / 65535.0;
    let (r, g, b) = hsv_to_rgb(h, 0.65, 0.95);
    Color::srgba(r, g, b, 0.35)
}

fn hsv_to_rgb(h: f32, s: f32, v: f32) -> (f32, f32, f32) {
    let i = (h * 6.0).floor() as i32;
    let f = h * 6.0 - i as f32;
    let p = v * (1.0 - s);
    let q = v * (1.0 - f * s);
    let t = v * (1.0 - (1.0 - f) * s);
    match i.rem_euclid(6) {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    }
}

/// Draws each chunk as a filled square (outlined) tinted by its connectivity
/// component id at the current view-Z (or surface band 0 when peeking).
pub fn connectivity_component_gizmo_system(
    overlay: Res<PathDebugOverlay>,
    conn: Res<ChunkConnectivity>,
    view_z: Res<CameraViewZ>,
    camera_query: Query<(&Transform, &OrthographicProjection), With<Camera>>,
    windows: Query<&Window>,
    mut gizmos: Gizmos,
) {
    if !overlay.show_connectivity_components {
        return;
    }
    let Ok((cam_transform, projection)) = camera_query.get_single() else {
        return;
    };
    let Ok(window) = windows.get_single() else {
        return;
    };

    let view_band = if view_z.0 == i32::MAX {
        0i8
    } else {
        crate::pathfinding::connectivity::z_band(view_z.0 as i8)
    };

    let half_w = window.width() * 0.5 * projection.scale;
    let half_h = window.height() * 0.5 * projection.scale;
    let cam = cam_transform.translation.truncate();
    let chunk_world = CHUNK_SIZE as f32 * TILE_SIZE;
    let cx_min = ((cam.x - half_w) / chunk_world).floor() as i32 - 1;
    let cx_max = ((cam.x + half_w) / chunk_world).ceil() as i32 + 1;
    let cy_min = ((cam.y - half_h) / chunk_world).floor() as i32 - 1;
    let cy_max = ((cam.y + half_h) / chunk_world).ceil() as i32 + 1;

    let half = chunk_world * 0.5;
    for (coord, band, id) in conn.iter() {
        if band != view_band {
            continue;
        }
        if coord.0 < cx_min || coord.0 > cx_max || coord.1 < cy_min || coord.1 > cy_max {
            continue;
        }
        let c = chunk_center(coord);
        let color = component_color(id);
        gizmos.rect_2d(c, Vec2::splat(chunk_world - 2.0), color);
        // Bright corner ticks make components visually pop even when the
        // tint is dim against bright terrain.
        let tick = 4.0;
        let tl = c + Vec2::new(-half, half);
        let tr = c + Vec2::new(half, half);
        let bl = c + Vec2::new(-half, -half);
        let br = c + Vec2::new(half, -half);
        let tick_color = Color::srgba(
            color.to_srgba().red,
            color.to_srgba().green,
            color.to_srgba().blue,
            0.95,
        );
        gizmos.line_2d(tl, tl + Vec2::new(tick, 0.0), tick_color);
        gizmos.line_2d(tr, tr + Vec2::new(-tick, 0.0), tick_color);
        gizmos.line_2d(bl, bl + Vec2::new(tick, 0.0), tick_color);
        gizmos.line_2d(br, br + Vec2::new(-tick, 0.0), tick_color);
    }
}

/// Red lines from start → goal for each entry in `FailureLog::recent`.
/// Z-filtered to whatever band the camera is currently looking at.
pub fn recent_failures_gizmo_system(
    overlay: Res<PathDebugOverlay>,
    failures: Res<FailureLog>,
    view_z: Res<CameraViewZ>,
    mut gizmos: Gizmos,
) {
    if !overlay.show_recent_failures {
        return;
    }
    let viewing_surface = view_z.0 == i32::MAX;
    let line_color = Color::srgba(1.0, 0.2, 0.2, 0.7);
    let goal_color = Color::srgba(1.0, 0.2, 0.2, 0.95);

    for rec in failures.recent.iter() {
        if !viewing_surface {
            // Show only failures whose start or goal is near the view-Z
            // band, otherwise the overlay is meaningless underground.
            let band = crate::pathfinding::connectivity::z_band(view_z.0 as i8);
            let sb = crate::pathfinding::connectivity::z_band(rec.start.2);
            let gb = crate::pathfinding::connectivity::z_band(rec.goal.2);
            if sb != band && gb != band {
                continue;
            }
        }
        let s = tile_center(rec.start.0, rec.start.1);
        let g = tile_center(rec.goal.0, rec.goal.1);
        gizmos.line_2d(s, g, line_color);
        gizmos.circle_2d(g, 3.0, goal_color);
        // Small X at the start.
        let r = 2.5;
        gizmos.line_2d(s + Vec2::new(-r, -r), s + Vec2::new(r, r), goal_color);
        gizmos.line_2d(s + Vec2::new(-r, r), s + Vec2::new(r, -r), goal_color);
    }
}

/// Like `recent_failures_gizmo_system` but filtered to the selected agent
/// only and with a brighter highlight color so it reads as "this agent's
/// recent failure history".
pub fn selected_agent_failures_gizmo_system(
    overlay: Res<PathDebugOverlay>,
    failures: Res<FailureLog>,
    selected: Res<SelectedEntity>,
    mut gizmos: Gizmos,
) {
    if !overlay.show_selected_failures {
        return;
    }
    let Some(entity) = selected.0 else { return };
    let highlight = Color::srgba(1.0, 0.85, 0.2, 0.9);

    for rec in failures.for_agent(entity) {
        let s = tile_center(rec.start.0, rec.start.1);
        let g = tile_center(rec.goal.0, rec.goal.1);
        gizmos.line_2d(s, g, highlight);
        gizmos.circle_2d(g, 4.0, highlight);
        let r = 3.5;
        gizmos.line_2d(s + Vec2::new(-r, -r), s + Vec2::new(r, r), highlight);
        gizmos.line_2d(s + Vec2::new(-r, r), s + Vec2::new(r, -r), highlight);
    }
}
