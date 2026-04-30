use ahash::AHashMap;
use bevy::prelude::*;
use std::cmp::Reverse;
use std::collections::BinaryHeap;

/// Reusable scratch buffers for one A* search. Allocated once and cleared
/// between calls — `BinaryHeap::clear` and `AHashMap::clear` keep capacity,
/// so steady-state A* allocates nothing.
#[derive(Default)]
pub struct AStarScratch {
    pub open: BinaryHeap<Reverse<(u32, (i32, i32, i8))>>,
    pub g_score: AHashMap<(i32, i32, i8), u32>,
    pub came_from: AHashMap<(i32, i32, i8), (i32, i32, i8)>,
}

impl AStarScratch {
    pub fn reset(&mut self) {
        self.open.clear();
        self.g_score.clear();
        self.came_from.clear();
    }
}

/// Pool of A* scratch buffers. The movement system borrows one mutably for
/// the duration of its tick; the path-request worker (added later) may
/// borrow another. Vec lets us grow on demand without contention.
#[derive(Resource, Default)]
pub struct AStarPool {
    scratches: Vec<AStarScratch>,
    pub high_water: usize,
}

impl AStarPool {
    /// Borrow a clean scratch by index. The same index always returns the
    /// same buffer, so callers in different systems should pick distinct
    /// indices (movement uses 0; worker uses 1; etc.).
    pub fn scratch(&mut self, index: usize) -> &mut AStarScratch {
        while self.scratches.len() <= index {
            self.scratches.push(AStarScratch::default());
        }
        if index + 1 > self.high_water {
            self.high_water = index + 1;
        }
        let s = &mut self.scratches[index];
        s.reset();
        s
    }

    /// Grow the pool until at least `n` scratches are available. Used by
    /// the parallel pathfinding worker before splitting the pool into
    /// per-task scratch slots.
    pub fn ensure(&mut self, n: usize) {
        while self.scratches.len() < n {
            self.scratches.push(AStarScratch::default());
        }
        if n > self.high_water {
            self.high_water = n;
        }
    }

    /// Borrow the first `n` scratches as a mutable slice for parallel use.
    /// Each scratch is reset before return so callers can hand each one
    /// directly to a spawned A* task. Caller must invoke `ensure(n)` first.
    pub fn slice_mut(&mut self, n: usize) -> &mut [AStarScratch] {
        let slice = &mut self.scratches[..n];
        for s in slice.iter_mut() {
            s.reset();
        }
        slice
    }
}
