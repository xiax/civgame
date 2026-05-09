//! Nomadic-mode systems: migration trigger, pack-camp orchestration, arrival.
//!
//! Phase 1-3 leave this module as a stub holder for type definitions that
//! Phase 8 fills in. Currently it carries only the `MigrationStage` /
//! `MigrationOrder` shapes that `FactionData::pending_migration` will hold;
//! the trigger, dispatch, and commit systems land in Phase 8.

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MigrationStage {
    /// Chief and band are tearing down old camp + loading pack animals.
    PackingCamp,
    /// Band on the move toward `target_tile`.
    EnRoute,
    /// Reached destination; deploying packed shelter and pack-bundles.
    Arrived,
}

#[derive(Clone, Debug)]
pub struct MigrationOrder {
    pub target_tile: (i32, i32),
    pub stage: MigrationStage,
    pub started_tick: u32,
}
