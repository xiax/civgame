use bevy::prelude::*;
use rand::Rng;
use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE};
use crate::world::terrain::{tile_to_world, TILE_SIZE, WORLD_CHUNKS_X, WORLD_CHUNKS_Y};
use crate::world::spatial::SpatialIndex;
use crate::pathfinding::chunk_graph::ChunkGraph;
use super::person::{AiState, PersonAI};
use super::lod::LodLevel;

const MOVE_SPEED: f32 = 48.0; // pixels per second
const IDLE_WANDER_INTERVAL: f32 = 2.5; // seconds between random moves

#[derive(Component, Default)]
pub struct MovementState {
    pub wander_timer: f32,
}

pub fn movement_system(
    time: Res<Time>,
    chunk_map: Res<ChunkMap>,
    chunk_graph: Res<ChunkGraph>,
    mut query: Query<(
        Entity,
        &mut Transform,
        &mut PersonAI,
        &LodLevel,
        &mut MovementState,
    )>,
) {
    let dt = time.delta_secs();
    let total_tiles_x = WORLD_CHUNKS_X * CHUNK_SIZE as i32;
    let total_tiles_y = WORLD_CHUNKS_Y * CHUNK_SIZE as i32;

    // Movement can't be fully parallel because it writes Transform (position sync)
    // and can read ChunkMap for passability. Run sequentially.
    for (_, mut transform, mut ai, lod, mut mv) in query.iter_mut() {
        if *lod == LodLevel::Dormant {
            continue;
        }

        let pos = transform.translation.truncate();
        let target_world = tile_to_world(ai.target_tile.0 as i32, ai.target_tile.1 as i32);
        let to_target = target_world - pos;
        let dist = to_target.length();

        if dist > 2.0 {
            // Move toward target
            let dir = to_target.normalize();
            let step = dir * MOVE_SPEED * dt;
            let new_pos = pos + step;
            transform.translation.x = new_pos.x;
            transform.translation.y = new_pos.y;
        } else {
            // Arrived at target
            transform.translation.x = target_world.x;
            transform.translation.y = target_world.y;

            match ai.state {
                AiState::Seeking => {
                    // Arrived at job target — start working
                    ai.state = AiState::Working;
                }
                AiState::Working => {
                    // Production system handles output; just accumulate progress.
                    ai.work_progress = ai.work_progress.saturating_add(1);
                }
                AiState::Idle => {
                    // Random wander
                    mv.wander_timer -= dt;
                    if mv.wander_timer <= 0.0 {
                        mv.wander_timer = IDLE_WANDER_INTERVAL;

                        let cur_tx = (pos.x / TILE_SIZE).floor() as i32;
                        let cur_ty = (pos.y / TILE_SIZE).floor() as i32;

                        let mut rng = rand::thread_rng();
                        let dirs: [(i32, i32); 8] = [
                            (-1, 0), (1, 0), (0, -1), (0, 1),
                            (-1, -1), (1, -1), (-1, 1), (1, 1),
                        ];
                        // Try random adjacent tile
                        let mut candidates: Vec<_> = dirs.iter().collect();
                        // Shuffle by picking random start index
                        let start = rng.gen_range(0..8);
                        let (left, right) = candidates.split_at(start);
                        let shuffled: Vec<_> = right.iter().chain(left.iter()).collect();

                        for &&(dx, dy) in &shuffled {
                            let ntx = (cur_tx + dx).clamp(0, total_tiles_x - 1);
                            let nty = (cur_ty + dy).clamp(0, total_tiles_y - 1);
                            if chunk_map.is_passable(ntx, nty) {
                                ai.target_tile = (ntx as i16, nty as i16);
                                break;
                            }
                        }
                    }
                }
                AiState::Sleeping | AiState::Attacking => {}
                AiState::Routing => {
                    // Arrived at a chunk-border waypoint; advance to next waypoint
                    // or switch to Seeking once we're in the destination chunk.
                    let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
                    let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
                    let dest_chunk = ChunkCoord(
                        (ai.target_tile.0 as i32).div_euclid(CHUNK_SIZE as i32),
                        (ai.target_tile.1 as i32).div_euclid(CHUNK_SIZE as i32),
                    );
                    let cur_chunk = ChunkCoord(
                        cur_tx.div_euclid(CHUNK_SIZE as i32),
                        cur_ty.div_euclid(CHUNK_SIZE as i32),
                    );
                    if cur_chunk == dest_chunk {
                        ai.state = AiState::Seeking;
                    } else if let Some(next_wp) =
                        chunk_graph.next_waypoint(cur_chunk, dest_chunk, &chunk_map)
                    {
                        ai.target_tile = next_wp;
                    } else {
                        // No route found — give up
                        ai.state = AiState::Idle;
                        ai.job_id = PersonAI::UNEMPLOYED;
                    }
                }
            }
        }
    }
}

pub fn update_spatial_index_system(
    mut index: ResMut<SpatialIndex>,
    query: Query<(Entity, &Transform)>,
) {
    index.0.clear();
    for (entity, transform) in &query {
        let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        index.insert(tx, ty, entity);
    }
}
