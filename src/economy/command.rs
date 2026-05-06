use bevy::prelude::*;

/// Maximum number of resource slots in the command-mode central pools.
/// Currently sized to comfortably hold the founding 22 resources; bump
/// if the catalog grows past this.
const COMMAND_POOL_SIZE: usize = 32;

/// Centrally-held resource pools and task quotas for command economy mode.
/// Indexed by `ResourceId.0 as usize`; new resources past `COMMAND_POOL_SIZE`
/// are silently ignored on deposit/withdraw.
#[derive(Resource, Default)]
pub struct CommandPools {
    pub stockpile: [f32; COMMAND_POOL_SIZE],
    pub quotas: [u32; COMMAND_POOL_SIZE],
}

impl CommandPools {
    pub fn deposit(&mut self, good_idx: usize, qty: f32) {
        if good_idx < COMMAND_POOL_SIZE {
            self.stockpile[good_idx] += qty;
        }
    }

    pub fn withdraw(&mut self, good_idx: usize, qty: f32) -> f32 {
        if good_idx < COMMAND_POOL_SIZE {
            let taken = self.stockpile[good_idx].min(qty);
            self.stockpile[good_idx] -= taken;
            taken
        } else {
            0.0
        }
    }
}
