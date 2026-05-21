use ahash::AHashSet;
use bevy::prelude::*;
use bevy::tasks::{block_on, futures_lite::future, AsyncComputeTaskPool, Task};
use std::collections::VecDeque;
use std::time::Instant;

use crate::simulation::perf::{micros_u32, BackgroundWorkDiagnostics, PerfWorkBudget};
use crate::simulation::person::Person;
use crate::world::chunk::{ChunkMap, CHUNK_SIZE};
use crate::world::globe::{Biome, Globe, GLOBE_HEIGHT, GLOBE_WIDTH};
use crate::world::seasons::{Calendar, Season};
use crate::world::terrain::TILE_SIZE;

#[derive(Resource, Default)]
pub struct WorldSimCursor {
    pub next_index: usize,
    pub generation: u64,
}

#[derive(Resource, Default)]
pub struct WorldSimTaskState {
    task: Option<Task<WorldSimResult>>,
    pending_deltas: VecDeque<WorldSimCellDelta>,
    pending_raids: VecDeque<RaidIntent>,
}

#[derive(Clone, Copy)]
struct WorldSimCellSnapshot {
    gx: i32,
    gy: i32,
    biome: Biome,
    faction_id: u32,
    population: u16,
    food_stock: f32,
}

#[derive(Clone, Copy)]
struct WorldSimCellDelta {
    gx: i32,
    gy: i32,
    faction_id: u32,
    population: u16,
    food_stock: f32,
}

#[derive(Clone, Copy)]
struct RaidIntent {
    raider_pos: (i32, i32),
    target_pos: (i32, i32),
}

struct WorldSimResult {
    generation: u64,
    snapshot_cells: usize,
    deltas: Vec<WorldSimCellDelta>,
    raids: Vec<RaidIntent>,
    elapsed: std::time::Duration,
}

/// Marks globe cells as explored when any agent is present in them.
pub fn agent_exploration_system(mut globe: ResMut<Globe>, agents: Query<&Transform, With<Person>>) {
    for transform in agents.iter() {
        let tile_x = (transform.translation.x / TILE_SIZE).floor() as i32;
        let tile_y = (transform.translation.y / TILE_SIZE).floor() as i32;
        let cx = tile_x.div_euclid(CHUNK_SIZE as i32);
        let cy = tile_y.div_euclid(CHUNK_SIZE as i32);
        let (gx, gy) = Globe::cell_for_chunk(cx, cy);
        if let Some(cell) = globe.cell_mut(gx, gy) {
            cell.explored = true;
        }
    }
}

/// Simplified simulation for globe cells not covered by any loaded chunk.
/// Advances a cursor through the globe and computes each batch off-thread.
pub fn world_sim_system(
    calendar: Res<Calendar>,
    mut globe: ResMut<Globe>,
    chunk_map: Res<ChunkMap>,
    mut cursor: ResMut<WorldSimCursor>,
    mut state: ResMut<WorldSimTaskState>,
    budget: Res<PerfWorkBudget>,
    mut perf: ResMut<BackgroundWorkDiagnostics>,
) {
    let season_mult: f32 = match calendar.season {
        Season::Spring => 0.8,
        Season::Summer => 1.0,
        Season::Autumn => 0.9,
        Season::Winter => 0.2,
    };

    if let Some(task) = state.task.as_mut() {
        if let Some(result) = block_on(future::poll_once(task)) {
            state.task = None;
            perf.world_sim_in_flight = false;
            perf.world_sim_compute_us = micros_u32(result.elapsed);
            perf.world_sim_snapshot_cells = result.snapshot_cells.min(u32::MAX as usize) as u32;
            if !queue_world_sim_result_if_current(&mut state, result, cursor.generation) {
                perf.world_sim_dropped_stale = perf.world_sim_dropped_stale.saturating_add(1);
            }
        } else {
            perf.world_sim_in_flight = true;
        }
    } else {
        perf.world_sim_in_flight = false;
    }

    let apply_started = Instant::now();
    let mut applied = 0u32;
    for _ in 0..budget.world_sim_deltas_per_tick {
        let Some(delta) = state.pending_deltas.pop_front() else {
            break;
        };
        let Some(cell) = globe.cell_mut(delta.gx, delta.gy) else {
            continue;
        };
        if cell.faction_id != delta.faction_id {
            continue;
        }
        cell.population = delta.population;
        cell.food_stock = delta.food_stock;
        applied = applied.saturating_add(1);
    }

    while let Some(raid) = state.pending_raids.pop_front() {
        let target_ok = globe
            .cell(raid.target_pos.0, raid.target_pos.1)
            .map(|c| c.food_stock > 10.0 && c.faction_id != 0)
            .unwrap_or(false);
        if target_ok {
            if let Some(t) = globe.cell_mut(raid.target_pos.0, raid.target_pos.1) {
                t.food_stock -= 5.0;
            }
            if let Some(r) = globe.cell_mut(raid.raider_pos.0, raid.raider_pos.1) {
                r.food_stock += 5.0;
            }
        }
    }
    perf.world_sim_deltas_applied_last_tick = applied;
    perf.world_sim_apply_us = micros_u32(apply_started.elapsed());
    perf.world_sim_pending_results = state.pending_deltas.len().min(u32::MAX as usize) as u32;

    if state.task.is_some() {
        perf.world_sim_cursor = cursor.next_index.min(u32::MAX as usize) as u32;
        return;
    }

    // Collect globe cells that are currently streamed in
    let loaded_cells: AHashSet<(i32, i32)> = chunk_map
        .0
        .keys()
        .map(|c| Globe::cell_for_chunk(c.0, c.1))
        .collect();

    let total_cells = (GLOBE_WIDTH * GLOBE_HEIGHT) as usize;
    let batch_size = budget.world_sim_cells_per_task.max(1).min(total_cells);
    let start = cursor.next_index.min(total_cells.saturating_sub(1));
    let mut idx = start;
    let mut snapshots: Vec<WorldSimCellSnapshot> = Vec::with_capacity(batch_size);
    for _ in 0..batch_size {
        let gx = (idx % GLOBE_WIDTH as usize) as i32;
        let gy = (idx / GLOBE_WIDTH as usize) as i32;
        idx = (idx + 1) % total_cells;
        if loaded_cells.contains(&(gx, gy)) {
            continue;
        }
        let Some(cell) = globe.cell(gx, gy) else {
            continue;
        };
        snapshots.push(WorldSimCellSnapshot {
            gx,
            gy,
            biome: cell.biome,
            faction_id: cell.faction_id,
            population: cell.population,
            food_stock: cell.food_stock,
        });
    }
    cursor.next_index = idx;
    perf.world_sim_cursor = cursor.next_index.min(u32::MAX as usize) as u32;
    perf.world_sim_snapshot_cells = snapshots.len().min(u32::MAX as usize) as u32;

    let generation = cursor.generation;
    let pool = AsyncComputeTaskPool::get();
    state.task = Some(pool.spawn(async move {
        compute_world_sim_batch(snapshots, loaded_cells, season_mult, generation)
    }));
    perf.world_sim_in_flight = true;
}

fn queue_world_sim_result_if_current(
    state: &mut WorldSimTaskState,
    result: WorldSimResult,
    generation: u64,
) -> bool {
    if result.generation != generation {
        return false;
    }
    for delta in result.deltas {
        state.pending_deltas.push_back(delta);
    }
    for raid in result.raids {
        state.pending_raids.push_back(raid);
    }
    true
}

fn compute_world_sim_batch(
    snapshots: Vec<WorldSimCellSnapshot>,
    loaded_cells: AHashSet<(i32, i32)>,
    season_mult: f32,
    generation: u64,
) -> WorldSimResult {
    let started = Instant::now();
    let snapshot_cells = snapshots.len();
    let mut deltas: Vec<WorldSimCellDelta> = Vec::new();
    let mut raids: Vec<RaidIntent> = Vec::new();

    for snap in snapshots {
        if snap.faction_id == 0 {
            continue;
        }
        let mut food_stock = snap.food_stock + snap.biome.yield_rate() * season_mult;
        let consumption = snap.population as f32 * 0.01 * 60.0;
        food_stock = (food_stock - consumption).max(0.0);

        let mut population = snap.population;
        if food_stock > population as f32 * 5.0 && population < 1000 {
            population = population.saturating_add(1);
        } else if food_stock == 0.0 && population > 0 {
            population = population.saturating_sub(1);
        }

        deltas.push(WorldSimCellDelta {
            gx: snap.gx,
            gy: snap.gy,
            faction_id: snap.faction_id,
            population,
            food_stock,
        });

        if food_stock == 0.0 {
            for (dx, dy) in [(0i32, -1i32), (0, 1), (-1, 0), (1, 0)] {
                let nx = snap.gx + dx;
                let ny = snap.gy + dy;
                if nx < 0 || ny < 0 || nx >= GLOBE_WIDTH || ny >= GLOBE_HEIGHT {
                    continue;
                }
                if loaded_cells.contains(&(nx, ny)) {
                    continue;
                }
                raids.push(RaidIntent {
                    raider_pos: (snap.gx, snap.gy),
                    target_pos: (nx, ny),
                });
                break;
            }
        }
    }

    WorldSimResult {
        generation,
        snapshot_cells,
        deltas,
        raids,
        elapsed: started.elapsed(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn world_sim_compute_returns_only_claimed_cell_deltas() {
        let snapshots = vec![
            WorldSimCellSnapshot {
                gx: 0,
                gy: 0,
                biome: Biome::Grassland,
                faction_id: 7,
                population: 10,
                food_stock: 0.0,
            },
            WorldSimCellSnapshot {
                gx: 1,
                gy: 0,
                biome: Biome::Grassland,
                faction_id: 0,
                population: 10,
                food_stock: 100.0,
            },
        ];
        let result = compute_world_sim_batch(snapshots, AHashSet::default(), 1.0, 42);
        assert_eq!(result.generation, 42);
        assert_eq!(result.deltas.len(), 1);
        assert_eq!(result.deltas[0].gx, 0);
        assert_eq!(result.deltas[0].population, 9);
        assert_eq!(result.raids.len(), 1);
        assert_eq!(result.raids[0].target_pos, (0, 1));
    }

    #[test]
    fn stale_world_sim_results_are_not_queued() {
        let mut state = WorldSimTaskState::default();
        let result = WorldSimResult {
            generation: 1,
            snapshot_cells: 1,
            deltas: vec![WorldSimCellDelta {
                gx: 0,
                gy: 0,
                faction_id: 1,
                population: 1,
                food_stock: 2.0,
            }],
            raids: Vec::new(),
            elapsed: Duration::ZERO,
        };

        assert!(!queue_world_sim_result_if_current(&mut state, result, 2));
        assert!(state.pending_deltas.is_empty());
    }
}
