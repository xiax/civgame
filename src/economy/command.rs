use super::goods::GOOD_COUNT;
use bevy::prelude::*;

/// Centrally-held resource pools and task quotas for command economy mode.
#[derive(Resource, Default)]
pub struct CommandPools {
    pub stockpile: [f32; GOOD_COUNT],
    pub quotas: [u32; GOOD_COUNT],
}

impl CommandPools {
    pub fn deposit(&mut self, good_idx: usize, qty: f32) {
        if good_idx < GOOD_COUNT {
            self.stockpile[good_idx] += qty;
        }
    }

    pub fn withdraw(&mut self, good_idx: usize, qty: f32) -> f32 {
        if good_idx < GOOD_COUNT {
            let taken = self.stockpile[good_idx].min(qty);
            self.stockpile[good_idx] -= taken;
            taken
        } else {
            0.0
        }
    }
}
