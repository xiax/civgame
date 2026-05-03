//! Top-level lifecycle state for the game.
//!
//! - `GeneratingWorld`: globe gen in progress (currently runs synchronously
//!   during plugin build so this state is brief).
//! - `SpawnSelect`: world map shown; player picks a starting mega-chunk.
//! - `Playing`: sim + chunk streaming active.

use bevy::prelude::*;

#[derive(States, Clone, Copy, Eq, PartialEq, Hash, Debug, Default)]
pub enum GameState {
    #[default]
    SpawnSelect,
    Playing,
}

/// Pending spawn-cell choice from the spawn-select UI. Read once on
/// `OnEnter(Playing)` to position the camera and seed the player faction.
#[derive(Resource, Default, Debug, Clone, Copy)]
pub struct PendingSpawn(pub Option<(i32, i32)>);
