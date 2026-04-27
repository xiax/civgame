use super::animals::{Deer, Wolf};
use super::combat::{Body, Health};
use super::construction::Bed;
use super::items::GroundItem;
use super::lod::LodLevel;
use super::person::{AiState, Person, PersonAI};
use super::plants::Plant;
use super::schedule::{BucketSlot, SimClock};
use super::tasks::task_interacts_from_adjacent;
use crate::pathfinding::chunk_graph::ChunkGraph;
use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE};
use crate::world::spatial::SpatialIndex;
use crate::world::terrain::{tile_to_world, TILE_SIZE};
use bevy::prelude::*;
use rand::Rng;

const MOVE_SPEED: f32 = 48.0; // pixels per second
const IDLE_WANDER_INTERVAL: f32 = 2.5; // seconds between random moves

#[derive(Component, Default)]
pub struct MovementState {
    pub wander_timer: f32,
}

pub fn movement_system(
    time: Res<Time>,
    clock: Res<SimClock>,
    chunk_map: Res<ChunkMap>,
    chunk_graph: Res<ChunkGraph>,
    spatial_index: Res<SpatialIndex>,
    mut query: Query<(
        Entity,
        &mut Transform,
        &mut PersonAI,
        &LodLevel,
        &mut MovementState,
        &BucketSlot,
    )>,
) {
    let dt = time.delta_secs();
    let speed = clock.speed;
    let sim_dt = dt * clock.scale_factor();

    // Movement can't be fully parallel because it writes Transform (position sync)
    // and can read ChunkMap for passability. Run sequentially.
    for (_, mut transform, mut ai, lod, mut mv, slot) in query.iter_mut() {
        if *lod == LodLevel::Dormant {
            continue;
        }

        let pos = transform.translation.truncate();
        let target_world = tile_to_world(ai.target_tile.0 as i32, ai.target_tile.1 as i32);
        let to_target = target_world - pos;
        let dist = to_target.length();

        if dist > 2.0 {
            // Working agent stopped adjacent to resource — stay put and accumulate progress.
            if ai.state == AiState::Working {
                if clock.is_active(slot.0) {
                    let progress = (sim_dt * 20.0).max(0.0) as u8;
                    ai.work_progress = ai.work_progress.saturating_add(progress);
                }
                continue;
            }

            // Interaction tasks: switch to Working when ≤1 tile (Chebyshev) from dest_tile.
            if ai.state == AiState::Seeking && task_interacts_from_adjacent(ai.task_id) {
                let cur_tx = (pos.x / TILE_SIZE).floor() as i32;
                let cur_ty = (pos.y / TILE_SIZE).floor() as i32;
                let cheb = (cur_tx - ai.dest_tile.0 as i32)
                    .abs()
                    .max((cur_ty - ai.dest_tile.1 as i32).abs());
                if cheb <= 1 {
                    ai.state = AiState::Working;
                    continue;
                }
            }

            // Normal movement toward target_tile.
            let dir = to_target.normalize();
            let step = dir * MOVE_SPEED * dt * speed;
            let new_pos = pos + step;
            transform.translation.x = new_pos.x;
            transform.translation.y = new_pos.y;
        } else {
            // Arrived at target
            transform.translation.x = target_world.x;
            transform.translation.y = target_world.y;

            match ai.state {
                AiState::Seeking => {
                    // Arrived at task target — start working, unless another agent is here.
                    let tx = (target_world.x / TILE_SIZE).floor() as i32;
                    let ty = (target_world.y / TILE_SIZE).floor() as i32;
                    let tz = chunk_map.surface_z_at(tx, ty);
                    if spatial_index.agent_count(tx, ty, tz) > 1 {
                        // Nudge to an adjacent free tile and stay Seeking.
                        let dirs: [(i32, i32); 8] = [
                            (-1, 0),
                            (1, 0),
                            (0, -1),
                            (0, 1),
                            (-1, -1),
                            (1, -1),
                            (-1, 1),
                            (1, 1),
                        ];
                        let mut rng = rand::thread_rng();
                        let start = rng.gen_range(0..8);
                        let mut bumped = false;
                        for i in 0..8usize {
                            let (dx, dy) = dirs[(start + i) % 8];
                            let (ntx, nty) = (tx + dx, ty + dy);
                            if chunk_map.passable_step(tx, ty, ntx, nty) {
                                let ntz = chunk_map.surface_z_at(ntx, nty);
                                if !spatial_index.agent_occupied(ntx, nty, ntz) {
                                    ai.target_tile = (ntx as i16, nty as i16);
                                    bumped = true;
                                    break;
                                }
                            }
                        }
                        if !bumped {
                            ai.state = AiState::Working;
                        }
                        // else: stays Seeking toward the adjacent tile
                    } else {
                        ai.state = AiState::Working;
                    }
                }
                AiState::Working => {
                    // Production system handles output; only accumulate progress when bucket is active.
                    if clock.is_active(slot.0) {
                        let progress = (sim_dt * 20.0).max(0.0) as u8;
                        ai.work_progress = ai.work_progress.saturating_add(progress);
                    }
                }
                AiState::Idle => {
                    // Random wander
                    mv.wander_timer -= dt * speed;
                    if mv.wander_timer <= 0.0 {
                        mv.wander_timer = IDLE_WANDER_INTERVAL;

                        let cur_tx = (pos.x / TILE_SIZE).floor() as i32;
                        let cur_ty = (pos.y / TILE_SIZE).floor() as i32;

                        let mut rng = rand::thread_rng();
                        let dirs: [(i32, i32); 8] = [
                            (-1, 0),
                            (1, 0),
                            (0, -1),
                            (0, 1),
                            (-1, -1),
                            (1, -1),
                            (-1, 1),
                            (1, 1),
                        ];
                        // Try random adjacent tile
                        let candidates: Vec<_> = dirs.iter().collect();
                        // Shuffle by picking random start index
                        let start = rng.gen_range(0..8);
                        let (left, right) = candidates.split_at(start);
                        let shuffled: Vec<_> = right.iter().chain(left.iter()).collect();

                        for &&(dx, dy) in &shuffled {
                            let ntx = cur_tx + dx;
                            let nty = cur_ty + dy;
                            let ntz = chunk_map.surface_z_at(ntx, nty);
                            if chunk_map.passable_step(cur_tx, cur_ty, ntx, nty)
                                && !spatial_index.agent_occupied(ntx, nty, ntz)
                            {
                                ai.target_tile = (ntx as i16, nty as i16);
                                ai.dest_tile = ai.target_tile;
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
                        (ai.dest_tile.0 as i32).div_euclid(CHUNK_SIZE as i32),
                        (ai.dest_tile.1 as i32).div_euclid(CHUNK_SIZE as i32),
                    );
                    let cur_chunk = ChunkCoord(
                        cur_tx.div_euclid(CHUNK_SIZE as i32),
                        cur_ty.div_euclid(CHUNK_SIZE as i32),
                    );

                    if cur_chunk == dest_chunk {
                        ai.state = AiState::Seeking;
                        ai.target_tile = ai.dest_tile;
                    } else if let Some(next_wp) =
                        chunk_graph.next_waypoint(cur_chunk, dest_chunk, &chunk_map)
                    {
                        ai.target_tile = next_wp;
                    } else {
                        // No route found — try to head toward destination anyway
                        ai.state = AiState::Seeking;
                        ai.target_tile = ai.dest_tile;
                    }
                }
            }
        }
    }
}

pub fn update_spatial_index_system(
    mut index: ResMut<SpatialIndex>,
    chunk_map: Res<ChunkMap>,
    query: Query<
        (
            Entity,
            &Transform,
            Option<&Health>,
            Option<&Body>,
            Has<Person>,
            Has<Wolf>,
            Has<Deer>,
        ),
        Or<(
            With<Person>,
            With<Wolf>,
            With<Deer>,
            With<Plant>,
            With<GroundItem>,
            With<Bed>,
        )>,
    >,
) {
    index.map.clear();
    index.agent_counts.clear();
    for (entity, transform, health, body, is_person, is_wolf, is_deer) in &query {
        let is_dead = health.map_or(false, |h| h.is_dead()) || body.map_or(false, |b| b.is_dead());
        if is_dead {
            continue;
        }

        let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        index.insert(tx, ty, entity);

        if is_person || is_wolf || is_deer {
            let tz = chunk_map.surface_z_at(tx, ty);
            *index.agent_counts.entry((tx, ty, tz)).or_insert(0) += 1;
        }
    }
}
