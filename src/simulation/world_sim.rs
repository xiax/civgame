use ahash::AHashSet;
use bevy::prelude::*;

use crate::simulation::person::Person;
use crate::simulation::schedule::SimClock;
use crate::world::chunk::{ChunkMap, CHUNK_SIZE};
use crate::world::globe::{Globe, GLOBE_HEIGHT, GLOBE_WIDTH};
use crate::world::seasons::{Calendar, Season};
use crate::world::terrain::TILE_SIZE;

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
/// Runs every 60 ticks.
pub fn world_sim_system(
    clock: Res<SimClock>,
    calendar: Res<Calendar>,
    mut globe: ResMut<Globe>,
    chunk_map: Res<ChunkMap>,
) {
    if clock.tick % 60 != 0 {
        return;
    }

    let season_mult: f32 = match calendar.season {
        Season::Spring => 0.8,
        Season::Summer => 1.0,
        Season::Autumn => 0.9,
        Season::Winter => 0.2,
    };

    // Collect globe cells that are currently streamed in
    let loaded_cells: AHashSet<(i32, i32)> = chunk_map
        .0
        .keys()
        .map(|c| Globe::cell_for_chunk(c.0, c.1))
        .collect();

    // Collect starving raider cells and their targets (avoid double-mut borrow)
    let mut raids: Vec<((i32, i32), (i32, i32))> = Vec::new();

    for gy in 0..GLOBE_HEIGHT {
        for gx in 0..GLOBE_WIDTH {
            if loaded_cells.contains(&(gx, gy)) {
                continue;
            }
            let cell = match globe.cell_mut(gx, gy) {
                Some(c) => c,
                None => continue,
            };
            if cell.faction_id == 0 {
                continue; // unclaimed — no population to simulate
            }

            // Food production
            cell.food_stock += cell.biome.yield_rate() * season_mult;

            // Food consumption: 0.01 per population per tick-batch (60 ticks)
            let consumption = cell.population as f32 * 0.01 * 60.0;
            cell.food_stock = (cell.food_stock - consumption).max(0.0);

            // Population dynamics
            if cell.food_stock > cell.population as f32 * 5.0 && cell.population < 1000 {
                cell.population += 1;
            } else if cell.food_stock == 0.0 && cell.population > 0 {
                cell.population -= 1;
            }

            // Queue raid on adjacent cell if starving
            if cell.food_stock == 0.0 {
                for (dx, dy) in [(0i32, -1i32), (0, 1), (-1, 0), (1, 0)] {
                    let nx = gx + dx;
                    let ny = gy + dy;
                    if nx < 0 || ny < 0 || nx >= GLOBE_WIDTH || ny >= GLOBE_HEIGHT {
                        continue;
                    }
                    if loaded_cells.contains(&(nx, ny)) {
                        continue;
                    }
                    raids.push(((gx, gy), (nx, ny)));
                    break;
                }
            }
        }
    }

    // Execute raids
    for (raider_pos, target_pos) in raids {
        let target_ok = globe
            .cell(target_pos.0, target_pos.1)
            .map(|c| c.food_stock > 10.0 && c.faction_id != 0)
            .unwrap_or(false);
        if target_ok {
            if let Some(t) = globe.cell_mut(target_pos.0, target_pos.1) {
                t.food_stock -= 5.0;
            }
            if let Some(r) = globe.cell_mut(raider_pos.0, raider_pos.1) {
                r.food_stock += 5.0;
            }
        }
    }
}
