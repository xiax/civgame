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

/// Economy preset selected at game start; applied to every faction's
/// `economic_policy` map by `spawn_population`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EconomyPreset {
    /// Default — empty `economic_policy` map (chief allocates everything).
    Subsistence,
    /// Chief still allocates food/wood/stone; tools/cloth/luxury resources
    /// flip `private_actors_allowed = true` so households can craft them.
    Mixed,
    /// Full `ResourceControlPolicy::capitalist()` on every catalog resource.
    Market,
}

/// Player-configurable game-start options written by the spawn-select UI
/// and read once by `spawn_population` (and `seed_starting_buildings`).
#[derive(Resource, Clone, Copy, Debug)]
pub struct GameStartOptions {
    pub era: crate::simulation::technology::Era,
    /// Number of starting members in the player faction (`group_idx == 0`).
    /// Other factions stay at the hardcoded `GROUP_SIZE`.
    pub player_population: u32,
    pub economy: EconomyPreset,
    /// Whether to pre-build era-appropriate structures around each faction's
    /// home tile. Set to false in sandbox mode.
    pub seed_buildings: bool,
}

impl Default for GameStartOptions {
    fn default() -> Self {
        Self {
            era: crate::simulation::technology::Era::Paleolithic,
            player_population: 20,
            economy: EconomyPreset::Subsistence,
            seed_buildings: true,
        }
    }
}
