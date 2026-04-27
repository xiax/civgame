use crate::world::chunk::Chunk;
use crate::world::chunk::{ChunkCoord, CHUNK_SIZE};
use ahash::AHashMap;
use bevy::prelude::*;

/// Direction bits: 0=N, 1=NE, 2=E, 3=SE, 4=S, 5=SW, 6=W, 7=NW, 0xFF=no path
pub struct FlowField {
    pub chunk: ChunkCoord,
    pub directions: Box<[u8; CHUNK_SIZE * CHUNK_SIZE]>,
    pub goal_tile: (u8, u8),
    pub generation: u32,
}

#[derive(Resource, Default)]
pub struct FlowFieldCache {
    pub fields: AHashMap<(ChunkCoord, u8, u8), FlowField>,
    pub max_cached: usize,
}

impl FlowFieldCache {
    pub fn get_or_build(&mut self, chunk: &Chunk, coord: ChunkCoord, goal: (u8, u8)) -> &FlowField {
        self.fields
            .entry((coord, goal.0, goal.1))
            .or_insert_with(|| build_flow_field(chunk, coord, goal))
    }
}

fn build_flow_field(chunk: &Chunk, coord: ChunkCoord, goal: (u8, u8)) -> FlowField {
    use std::collections::VecDeque;

    let mut dist = [u16::MAX; CHUNK_SIZE * CHUNK_SIZE];
    let mut dir = [0xFFu8; CHUNK_SIZE * CHUNK_SIZE];

    let goal_idx = goal.1 as usize * CHUNK_SIZE + goal.0 as usize;
    dist[goal_idx] = 0;
    dir[goal_idx] = 0;

    let mut queue = VecDeque::new();
    queue.push_back(goal);

    // (dx, dy, direction_bit_pointing_back_toward_goal)
    const NEIGHBORS: [(i32, i32, u8); 8] = [
        (0, 1, 4),
        (-1, 1, 3),
        (-1, 0, 2),
        (-1, -1, 1),
        (0, -1, 0),
        (1, -1, 7),
        (1, 0, 6),
        (1, 1, 5),
    ];

    while let Some((x, y)) = queue.pop_front() {
        let cur_dist = dist[y as usize * CHUNK_SIZE + x as usize];

        for &(dx, dy, d) in &NEIGHBORS {
            let nx = x as i32 + dx;
            let ny = y as i32 + dy;
            if nx < 0 || ny < 0 || nx >= CHUNK_SIZE as i32 || ny >= CHUNK_SIZE as i32 {
                continue;
            }
            let nidx = ny as usize * CHUNK_SIZE + nx as usize;
            if chunk.is_locally_passable(nx as usize, ny as usize) && dist[nidx] == u16::MAX {
                dist[nidx] = cur_dist + 1;
                dir[nidx] = d;
                queue.push_back((nx as u8, ny as u8));
            }
        }
    }

    FlowField {
        chunk: coord,
        directions: Box::new(dir),
        goal_tile: goal,
        generation: 0,
    }
}
