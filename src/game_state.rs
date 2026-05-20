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

/// Sub-state of `GameState::Playing` gating the unified settlement build
/// pipeline. `Warmup` covers the first ticks after `OnEnter(Playing)` while
/// the initial `SettlementBrain` survey + seed pass runs; per-tick agent
/// simulation is **not** gated on `Active` today (the agents still tick
/// during Warmup) but this state exists so future systems can opt in via
/// `.run_if(in_state(SimulationState::Active))` without further plumbing.
///
/// Flipped to `Active` by `mark_warmup_complete_system` once every settled
/// faction has had its initial survey applied and `seed_starting_buildings_system`
/// has run.
#[derive(SubStates, Clone, Copy, Eq, PartialEq, Hash, Debug, Default)]
#[source(GameState = GameState::Playing)]
pub enum SimulationState {
    #[default]
    Warmup,
    Active,
}

/// Pending spawn-cell choice from the spawn-select UI. Read once on
/// `OnEnter(Playing)` to position the camera and seed the player faction.
#[derive(Resource, Default, Debug, Clone, Copy)]
pub struct PendingSpawn(pub Option<(i32, i32)>);

/// User-facing seed driving globe + per-tile terrain generation. The
/// spawn-select UI exposes this as an editable field with Apply / Reroll
/// buttons; firing `RegenerateWorldRequest` rebuilds the `Globe` and
/// `WorldGen` resources from this seed.
#[derive(Resource, Clone, Copy, Debug)]
pub struct WorldSeed(pub u64);

impl Default for WorldSeed {
    fn default() -> Self {
        Self(42)
    }
}

/// Fired by the spawn-select UI to rebuild the world from `WorldSeed`.
#[derive(Event)]
pub struct RegenerateWorldRequest;

/// Civic seeding density at game start. Controls how aggressively
/// `seed_starting_buildings_system` bypasses the runtime `(Era, peak_pop)`
/// civic milestone gates.
///
/// - `Founder` — gates stay live; Neolithic-20 starts skip Market/Barracks/
///   Monument until they hit the population thresholds.
/// - `Established` (default) — current behaviour: every era-appropriate
///   civic (Granary/Shrine/Market/Barracks/Monument) seeds regardless of
///   pop, mirroring "society in progress".
/// - `Developed` — `Established` + explicit override that always emits
///   Monument/Barracks/Market once `era >= Chalcolithic`, even with a
///   smaller pop than the Bronze milestones would normally require.
#[derive(Resource, Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartSettlementMaturity {
    Founder,
    Established,
    Developed,
}

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
    /// Faction archetype for the player faction. Nomadic skips Settlement
    /// founding, plot carving, and FactionStorageTile spawn; structures use
    /// the pack/deploy cycle and the band migrates seasonally.
    pub lifestyle: crate::simulation::faction::Lifestyle,
    /// Civic seeding density. See `StartSettlementMaturity`.
    pub maturity: StartSettlementMaturity,
}

impl Default for GameStartOptions {
    fn default() -> Self {
        Self {
            era: crate::simulation::technology::Era::Paleolithic,
            player_population: 20,
            economy: EconomyPreset::Subsistence,
            seed_buildings: true,
            lifestyle: crate::simulation::faction::Lifestyle::Settled,
            maturity: StartSettlementMaturity::Established,
        }
    }
}
